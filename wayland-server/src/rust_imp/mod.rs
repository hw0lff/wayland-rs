use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use downcast_rs::Downcast;

use wayland_commons::debug;
use wayland_commons::map::ObjectMap;
use wayland_commons::wire::Message;
use wayland_commons::{MessageGroup, ThreadGuard};

use crate::{DispatchData, Filter, Interface, Main, Resource};

mod clients;
mod display;
mod event_loop_glue;
mod globals;
mod resources;

pub(crate) use self::clients::ClientInner;
pub(crate) use self::display::DisplayInner;
pub(crate) use self::globals::GlobalInner;
pub(crate) use self::resources::ResourceInner;

use self::resources::ResourceDestructor;

/// Flag to toggle debug output.
static WAYLAND_DEBUG: AtomicBool = AtomicBool::new(false);

/// A handle to the object map internal to the library state
///
/// This type is only used by code generated by `wayland-scanner`, and can not
/// be instantiated directly.
pub struct ResourceMap {
    map: Arc<Mutex<ObjectMap<self::resources::ObjectMeta>>>,
    client: ClientInner,
}

impl ResourceMap {
    fn make(
        map: Arc<Mutex<ObjectMap<self::resources::ObjectMeta>>>,
        client: ClientInner,
    ) -> ResourceMap {
        ResourceMap { map, client }
    }

    /// Returns the `Resource` corresponding to a given id
    pub fn get<I: Interface + From<Resource<I>> + AsRef<Resource<I>>>(
        &mut self,
        id: u32,
    ) -> Option<Resource<I>> {
        ResourceInner::from_id(id, self.map.clone(), self.client.clone()).map(|object| {
            debug_assert!(I::NAME == "<anonymous>" || object.is_interface::<I>());
            Resource::wrap(object)
        })
    }

    /// Creates a `NewResource` for a given id
    pub fn get_new<I: Interface + AsRef<Resource<I>> + From<Resource<I>>>(
        &mut self,
        id: u32,
    ) -> Option<Main<I>> {
        debug_assert!(self
            .map
            .lock()
            .unwrap()
            .find(id)
            .map(|obj| obj.is_interface::<I>())
            .unwrap_or(true));
        ResourceInner::from_id(id, self.map.clone(), self.client.clone()).map(Main::wrap)
    }
}

/*
 * Dispatching logic
 */
#[allow(clippy::large_enum_variant)]
pub(crate) enum Dispatched {
    Yes,
    NoDispatch(Message, ResourceInner),
    BadMsg,
}

pub(crate) trait Dispatcher: Downcast {
    fn dispatch(
        &mut self,
        msg: Message,
        resource: ResourceInner,
        map: &mut ResourceMap,
        data: DispatchData,
    ) -> Dispatched;
}

mod dispatcher_impl {
    // this mod has for sole purpose to allow to silence these `dead_code` warnings...
    #![allow(dead_code)]
    use super::Dispatcher;
    downcast_rs::impl_downcast!(Dispatcher);
}

pub(crate) struct ImplDispatcher<
    I: Interface + From<Resource<I>> + AsRef<Resource<I>>,
    F: FnMut(I::Request, Main<I>, DispatchData<'_>),
> {
    _i: ::std::marker::PhantomData<&'static I>,
    implementation: F,
}

impl<I, F> Dispatcher for ImplDispatcher<I, F>
where
    I: Interface + From<Resource<I>> + AsRef<Resource<I>>,
    F: FnMut(I::Request, Main<I>, DispatchData<'_>) + 'static,
    I::Request: MessageGroup<Map = ResourceMap>,
{
    fn dispatch(
        &mut self,
        msg: Message,
        resource: ResourceInner,
        map: &mut ResourceMap,
        data: DispatchData,
    ) -> Dispatched {
        let opcode = msg.opcode as usize;

        if WAYLAND_DEBUG.load(Ordering::Relaxed) {
            debug::print_dispatched_message(
                resource.object.interface,
                resource.id,
                resource.object.requests[opcode].name,
                &msg.args,
            );
        }

        let message = match I::Request::from_raw(msg, map) {
            Ok(msg) => msg,
            Err(_) => return Dispatched::BadMsg,
        };

        if message.since() > resource.version() {
            eprintln!(
                "Received an request {} requiring version >= {} while resource {}@{} is version {}.",
                resource.object.requests[opcode].name,
                message.since(),
                resource.object.interface,
                resource.id,
                resource.version()
            );
            return Dispatched::BadMsg;
        }

        if message.is_destructor() {
            resource.object.meta.alive.store(false, Ordering::Release);
            let mut kill = false;
            if let Some(ref mut data) = *resource.client.data.lock().unwrap() {
                data.schedule_destructor(resource.clone());
                kill = data.delete_id(resource.id).is_err();
            }
            if kill {
                resource.client.kill();
            }
        }

        (self.implementation)(message, Main::<I>::wrap(resource), data);

        Dispatched::Yes
    }
}

pub(crate) fn make_dispatcher<I, E>(filter: Filter<E>) -> Arc<ThreadGuard<RefCell<dyn Dispatcher>>>
where
    I: Interface + AsRef<Resource<I>> + From<Resource<I>>,
    E: From<(Main<I>, I::Request)> + 'static,
    I::Request: MessageGroup<Map = ResourceMap>,
{
    Arc::new(ThreadGuard::new(RefCell::new(ImplDispatcher {
        _i: ::std::marker::PhantomData,
        implementation: move |evt, res, data| filter.send((res, evt).into(), data),
    })))
}

pub(crate) fn default_dispatcher() -> Arc<ThreadGuard<RefCell<dyn Dispatcher>>> {
    struct DefaultDisp;
    impl Dispatcher for DefaultDisp {
        fn dispatch(
            &mut self,
            msg: Message,
            resource: ResourceInner,
            _map: &mut ResourceMap,
            _data: DispatchData,
        ) -> Dispatched {
            Dispatched::NoDispatch(msg, resource)
        }
    }

    Arc::new(ThreadGuard::new(RefCell::new(DefaultDisp)))
}

pub(crate) fn make_destructor<I, E>(filter: Filter<E>) -> Arc<ThreadGuard<ResourceDestructor>>
where
    I: Interface + AsRef<Resource<I>> + From<Resource<I>>,
    E: From<Resource<I>> + 'static,
{
    Arc::new(ThreadGuard::new(RefCell::new(move |res, data: DispatchData<'_>| {
        filter.send(Resource::<I>::wrap(res).into(), data)
    })))
}
