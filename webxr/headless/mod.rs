/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::SessionBuilder;
use crate::SwapChains;

use webxr_api::util::{self, ClipPlanes, HitTestList};
use webxr_api::ApiSpace;
use webxr_api::BaseSpace;
use webxr_api::DeviceAPI;
use webxr_api::DiscoveryAPI;
use webxr_api::Error;
use webxr_api::Event;
use webxr_api::EventBuffer;
use webxr_api::Floor;
use webxr_api::Frame;
use webxr_api::FrameUpdateEvent;
use webxr_api::HitTestId;
use webxr_api::HitTestResult;
use webxr_api::HitTestSource;
use webxr_api::Input;
use webxr_api::InputFrame;
use webxr_api::InputSource;
use webxr_api::MockDeviceInit;
use webxr_api::MockDeviceMsg;
use webxr_api::MockDiscoveryAPI;
use webxr_api::MockInputMsg;
use webxr_api::MockViewInit;
use webxr_api::MockViewsInit;
use webxr_api::MockWorld;
use webxr_api::Native;
use webxr_api::Quitter;
use webxr_api::Ray;
use webxr_api::Receiver;
use webxr_api::SelectEvent;
use webxr_api::Sender;
use webxr_api::Session;
use webxr_api::SessionInit;
use webxr_api::SessionMode;
use webxr_api::Space;
use webxr_api::View;
use webxr_api::Viewer;
use webxr_api::Views;

use euclid::RigidTransform3D;

use std::sync::{Arc, Mutex};
use std::thread;

use surfman::platform::generic::universal::surface::Surface;

pub struct HeadlessMockDiscovery {}

struct HeadlessDiscovery {
    data: Arc<Mutex<HeadlessDeviceData>>,
    supports_vr: bool,
    supports_inline: bool,
    supports_ar: bool,
}

struct InputInfo {
    source: InputSource,
    active: bool,
    pointer: Option<RigidTransform3D<f32, Input, Native>>,
    grip: Option<RigidTransform3D<f32, Input, Native>>,
    clicking: bool,
}

struct HeadlessDevice {
    data: Arc<Mutex<HeadlessDeviceData>>,
    mode: SessionMode,
    clip_planes: ClipPlanes,
    hit_tests: HitTestList,
    granted_features: Vec<String>,
}

struct HeadlessDeviceData {
    floor_transform: Option<RigidTransform3D<f32, Native, Floor>>,
    viewer_origin: Option<RigidTransform3D<f32, Viewer, Native>>,
    supported_features: Vec<String>,
    views: MockViewsInit,
    needs_view_update: bool,
    needs_floor_update: bool,
    inputs: Vec<InputInfo>,
    events: EventBuffer,
    quitter: Option<Quitter>,
    disconnected: bool,
    world: Option<MockWorld>,
}

impl MockDiscoveryAPI<SwapChains> for HeadlessMockDiscovery {
    fn simulate_device_connection(
        &mut self,
        init: MockDeviceInit,
        receiver: Receiver<MockDeviceMsg>,
    ) -> Result<Box<dyn DiscoveryAPI<SwapChains>>, Error> {
        let viewer_origin = init.viewer_origin.clone();
        let floor_transform = init.floor_origin.map(|f| f.inverse());
        let views = init.views.clone();
        let data = HeadlessDeviceData {
            floor_transform,
            viewer_origin,
            supported_features: init.supported_features,
            views,
            needs_view_update: false,
            needs_floor_update: false,
            inputs: vec![],
            events: Default::default(),
            quitter: None,
            disconnected: false,
            world: init.world,
        };
        let data = Arc::new(Mutex::new(data));
        let data_ = data.clone();

        thread::spawn(move || {
            run_loop(receiver, data_);
        });
        Ok(Box::new(HeadlessDiscovery {
            data,
            supports_vr: init.supports_vr,
            supports_inline: init.supports_inline,
            supports_ar: init.supports_ar,
        }))
    }
}

fn run_loop(receiver: Receiver<MockDeviceMsg>, data: Arc<Mutex<HeadlessDeviceData>>) {
    while let Ok(msg) = receiver.recv() {
        if !data.lock().expect("Mutex poisoned").handle_msg(msg) {
            break;
        }
    }
}

impl DiscoveryAPI<SwapChains> for HeadlessDiscovery {
    fn request_session(
        &mut self,
        mode: SessionMode,
        init: &SessionInit,
        xr: SessionBuilder,
    ) -> Result<Session, Error> {
        if !self.supports_session(mode) {
            return Err(Error::NoMatchingDevice);
        }
        let data = self.data.clone();
        let clip_planes = Default::default();
        let granted_features = init.validate(mode, &data.lock().unwrap().supported_features)?;
        xr.spawn(move || {
            Ok(HeadlessDevice {
                data,
                mode,
                clip_planes,
                granted_features,
                hit_tests: HitTestList::default(),
            })
        })
    }

    fn supports_session(&self, mode: SessionMode) -> bool {
        if self.data.lock().unwrap().disconnected {
            return false;
        }
        match mode {
            SessionMode::Inline => self.supports_inline,
            SessionMode::ImmersiveVR => self.supports_vr,
            SessionMode::ImmersiveAR => self.supports_ar,
        }
    }
}

fn view<Eye>(init: MockViewInit<Eye>, clip_planes: ClipPlanes) -> View<Eye> {
    let projection = if let Some((l, r, t, b)) = init.fov {
        util::fov_to_projection_matrix(l, r, t, b, clip_planes)
    } else {
        init.projection
    };

    View {
        transform: init.transform,
        projection,
        viewport: init.viewport,
    }
}

impl DeviceAPI<Surface> for HeadlessDevice {
    fn floor_transform(&self) -> Option<RigidTransform3D<f32, Native, Floor>> {
        self.data.lock().unwrap().floor_transform.clone()
    }

    fn views(&self) -> Views {
        if self.mode == SessionMode::Inline {
            Views::Inline
        } else {
            let views = self.data.lock().unwrap().views.clone();
            match views {
                MockViewsInit::Mono(one) => Views::Mono(view(one, self.clip_planes)),
                MockViewsInit::Stereo(one, two) => {
                    Views::Stereo(view(one, self.clip_planes), view(two, self.clip_planes))
                }
            }
        }
    }

    fn wait_for_animation_frame(&mut self) -> Option<Frame> {
        thread::sleep(std::time::Duration::from_millis(20));
        let mut data = self.data.lock().unwrap();
        let mut frame = data.get_frame();
        let events = self.hit_tests.commit_tests();
        frame.events = events;
        if data.needs_view_update {
            data.needs_view_update = false;
            frame
                .events
                .push(FrameUpdateEvent::UpdateViews(self.views()))
        };

        if let Some(ref world) = data.world {
            for source in self.hit_tests.tests() {
                let ray = data.native_ray(source.ray, source.space);
                let ray = if let Some(ray) = ray { ray } else { break };
                let hits = world
                    .regions
                    .iter()
                    .filter(|region| source.types.is_type(region.ty))
                    .flat_map(|region| &region.faces)
                    .filter_map(|triangle| triangle.intersect(ray))
                    .map(|space| HitTestResult {
                        space,
                        id: source.id,
                    });
                frame.hit_test_results.extend(hits);
            }
        }

        if data.needs_floor_update {
            frame.events.push(FrameUpdateEvent::UpdateFloorTransform(
                data.floor_transform.clone(),
            ));
            data.needs_floor_update = false;
        }
        Some(frame)
    }

    fn render_animation_frame(&mut self, surface: Surface) -> Surface {
        surface
    }

    fn initial_inputs(&self) -> Vec<InputSource> {
        vec![]
    }

    fn set_event_dest(&mut self, dest: Sender<Event>) {
        self.data.lock().unwrap().events.upgrade(dest)
    }

    fn quit(&mut self) {
        self.data.lock().unwrap().events.callback(Event::SessionEnd);
    }

    fn set_quitter(&mut self, quitter: Quitter) {
        self.data.lock().unwrap().quitter = Some(quitter);
    }

    fn update_clip_planes(&mut self, near: f32, far: f32) {
        self.clip_planes.update(near, far);
        self.data.lock().unwrap().needs_view_update = true;
    }

    fn granted_features(&self) -> &[String] {
        &self.granted_features
    }

    fn request_hit_test(&mut self, source: HitTestSource) {
        self.hit_tests.request_hit_test(source)
    }

    fn cancel_hit_test(&mut self, id: HitTestId) {
        self.hit_tests.cancel_hit_test(id)
    }
}

impl HeadlessMockDiscovery {
    pub fn new() -> HeadlessMockDiscovery {
        HeadlessMockDiscovery {}
    }
}

impl HeadlessDeviceData {
    fn get_frame(&self) -> Frame {
        let time_ns = time::precise_time_ns();
        let transform = self.viewer_origin;
        let inputs = self
            .inputs
            .iter()
            .filter(|i| i.active)
            .map(|i| InputFrame {
                id: i.source.id,
                target_ray_origin: i.pointer,
                grip_origin: i.grip,
                pressed: false,
                squeezed: false,
            })
            .collect();

        Frame {
            transform,
            inputs,
            events: vec![],
            time_ns,
            sent_time: 0,
            hit_test_results: vec![],
        }
    }

    fn handle_msg(&mut self, msg: MockDeviceMsg) -> bool {
        match msg {
            MockDeviceMsg::SetWorld(w) => self.world = Some(w),
            MockDeviceMsg::ClearWorld => self.world = None,
            MockDeviceMsg::SetViewerOrigin(viewer_origin) => {
                self.viewer_origin = viewer_origin;
            }
            MockDeviceMsg::SetFloorOrigin(floor_origin) => {
                self.floor_transform = floor_origin.map(|f| f.inverse());
                self.needs_floor_update = true;
            }
            MockDeviceMsg::SetViews(views) => {
                self.views = views;
                self.needs_view_update = true;
            }
            MockDeviceMsg::VisibilityChange(v) => self.events.callback(Event::VisibilityChange(v)),
            MockDeviceMsg::AddInputSource(init) => {
                self.inputs.push(InputInfo {
                    source: init.source.clone(),
                    pointer: init.pointer_origin,
                    grip: init.grip_origin,
                    active: true,
                    clicking: false,
                });
                self.events.callback(Event::AddInput(init.source))
            }
            MockDeviceMsg::MessageInputSource(id, msg) => {
                if let Some(ref mut input) = self.inputs.iter_mut().find(|i| i.source.id == id) {
                    match msg {
                        MockInputMsg::SetHandedness(h) => {
                            input.source.handedness = h;
                            self.events
                                .callback(Event::UpdateInput(id, input.source.clone()));
                        }
                        MockInputMsg::SetProfiles(p) => {
                            input.source.profiles = p;
                            self.events
                                .callback(Event::UpdateInput(id, input.source.clone()));
                        }
                        MockInputMsg::SetTargetRayMode(t) => {
                            input.source.target_ray_mode = t;
                            self.events
                                .callback(Event::UpdateInput(id, input.source.clone()));
                        }
                        MockInputMsg::SetPointerOrigin(p) => input.pointer = p,
                        MockInputMsg::SetGripOrigin(p) => input.grip = p,
                        MockInputMsg::TriggerSelect(kind, event) => {
                            if !input.active {
                                return true;
                            }
                            let clicking = input.clicking;
                            input.clicking = event == SelectEvent::Start;
                            let frame = self.get_frame();
                            match event {
                                SelectEvent::Start => {
                                    self.events.callback(Event::Select(id, kind, event, frame));
                                }
                                SelectEvent::End => {
                                    if clicking {
                                        self.events.callback(Event::Select(
                                            id,
                                            kind,
                                            SelectEvent::Select,
                                            frame,
                                        ));
                                    } else {
                                        self.events.callback(Event::Select(
                                            id,
                                            kind,
                                            SelectEvent::End,
                                            frame,
                                        ));
                                    }
                                }
                                SelectEvent::Select => {
                                    self.events.callback(Event::Select(
                                        id,
                                        kind,
                                        SelectEvent::Start,
                                        frame.clone(),
                                    ));
                                    self.events.callback(Event::Select(
                                        id,
                                        kind,
                                        SelectEvent::Select,
                                        frame,
                                    ));
                                }
                            }
                        }
                        MockInputMsg::Disconnect => {
                            if input.active {
                                self.events.callback(Event::RemoveInput(input.source.id));
                                input.active = false;
                                input.clicking = false;
                            }
                        }
                        MockInputMsg::Reconnect => {
                            if !input.active {
                                self.events.callback(Event::AddInput(input.source.clone()));
                                input.active = true;
                            }
                        }
                    }
                }
            }
            MockDeviceMsg::Disconnect(s) => {
                self.disconnected = true;
                self.quitter.as_ref().map(|q| q.quit());
                // notify the client that we're done disconnecting
                let _ = s.send(());
                return false;
            }
        }
        true
    }

    fn native_ray(&self, ray: Ray<ApiSpace>, space: Space) -> Option<Ray<Native>> {
        let origin: RigidTransform3D<f32, ApiSpace, Native> = match space.base {
            BaseSpace::Local => RigidTransform3D::identity(),
            BaseSpace::Floor => self.floor_transform?.inverse().cast_unit(),
            BaseSpace::Viewer => self.viewer_origin?.cast_unit(),
            BaseSpace::TargetRay(id) => self
                .inputs
                .iter()
                .find(|i| i.source.id == id)?
                .pointer?
                .cast_unit(),
            BaseSpace::Grip(id) => self
                .inputs
                .iter()
                .find(|i| i.source.id == id)?
                .grip?
                .cast_unit(),
        };
        let space_origin = origin.pre_transform(&space.offset);

        let origin_rigid: RigidTransform3D<f32, ApiSpace, ApiSpace> = ray.origin.into();
        Some(Ray {
            origin: origin_rigid.post_transform(&space_origin).translation,
            direction: space_origin.rotation.transform_vector3d(ray.direction),
        })
    }
}
