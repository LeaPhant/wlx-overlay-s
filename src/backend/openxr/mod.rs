use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use glam::{Affine3A, Vec3};
use openxr as xr;
use vulkano::{command_buffer::CommandBufferUsage, Handle, VulkanObject};

use crate::{
    backend::{
        common::{OverlayContainer, TaskType},
        input::interact,
        openxr::{input::DoubleClickCounter, lines::LinePool, overlay::OpenXrOverlayData},
    },
    graphics::WlxGraphics,
    overlays::watch::{watch_fade, WATCH_NAME},
    state::AppState,
};

use super::common::BackendError;

mod helpers;
mod input;
mod lines;
mod overlay;
mod swapchain;

const VIEW_TYPE: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;
const VIEW_COUNT: u32 = 2;

struct XrState {
    instance: xr::Instance,
    system: xr::SystemId,
    session: xr::Session<xr::Vulkan>,
    predicted_display_time: xr::Time,
    stage: Arc<xr::Space>,
}

pub fn openxr_run(running: Arc<AtomicBool>) -> Result<(), BackendError> {
    let (xr_instance, system) = match helpers::init_xr() {
        Ok((xr_instance, system)) => (xr_instance, system),
        Err(e) => {
            log::warn!("Will not use OpenXR: {}", e);
            return Err(BackendError::NotSupported);
        }
    };

    let environment_blend_mode = xr_instance
        .enumerate_environment_blend_modes(system, VIEW_TYPE)
        .unwrap()[0];
    log::info!("Using environment blend mode: {:?}", environment_blend_mode);

    let mut app_state = {
        let graphics = WlxGraphics::new_openxr(xr_instance.clone(), system);
        AppState::from_graphics(graphics)
    };

    let mut overlays = OverlayContainer::<OpenXrOverlayData>::new(&mut app_state);
    let mut lines = LinePool::new(app_state.graphics.clone());

    #[cfg(feature = "osc")]
    let mut osc_sender =
        crate::backend::osc::OscSender::new(app_state.session.config.osc_out_port).ok();

    app_state.hid_provider.set_desktop_extent(overlays.extent);

    let (session, mut frame_wait, mut frame_stream) = unsafe {
        let raw_session = helpers::create_overlay_session(
            &xr_instance,
            system,
            &xr::vulkan::SessionCreateInfo {
                instance: app_state.graphics.instance.handle().as_raw() as _,
                physical_device: app_state
                    .graphics
                    .device
                    .physical_device()
                    .handle()
                    .as_raw() as _,
                device: app_state.graphics.device.handle().as_raw() as _,
                queue_family_index: app_state.graphics.queue.queue_family_index(),
                queue_index: 0,
            },
        )
        .unwrap();
        xr::Session::from_raw(xr_instance.clone(), raw_session, Box::new(()))
    };

    let stage = session
        .create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)
        .unwrap();

    let mut xr_state = XrState {
        instance: xr_instance,
        system,
        session,
        predicted_display_time: xr::Time::from_nanos(0),
        stage: Arc::new(stage),
    };

    let pointer_lines = [
        lines.allocate(&xr_state, app_state.graphics.clone()),
        lines.allocate(&xr_state, app_state.graphics.clone()),
    ];

    let watch_id = overlays.get_by_name(WATCH_NAME).unwrap().state.id;

    let input_source = input::OpenXrInputSource::new(&xr_state);

    let mut session_running = false;
    let mut event_storage = xr::EventDataBuffer::new();

    let mut show_hide_counter = DoubleClickCounter::new();
    let mut due_tasks = VecDeque::with_capacity(4);

    'main_loop: loop {
        if !running.load(Ordering::Relaxed) {
            log::warn!("Received shutdown signal.");
            match xr_state.session.request_exit() {
                Ok(_) => log::info!("OpenXR session exit requested."),
                Err(xr::sys::Result::ERROR_SESSION_NOT_RUNNING) => break 'main_loop,
                Err(e) => {
                    log::error!("Failed to request OpenXR session exit: {}", e);
                    break 'main_loop;
                }
            }
        }

        while let Some(event) = xr_state.instance.poll_event(&mut event_storage).unwrap() {
            use xr::Event::*;
            match event {
                SessionStateChanged(e) => {
                    // Session state change is where we can begin and end sessions, as well as
                    // find quit messages!
                    println!("entered state {:?}", e.state());
                    match e.state() {
                        xr::SessionState::READY => {
                            xr_state.session.begin(VIEW_TYPE).unwrap();
                            session_running = true;
                        }
                        xr::SessionState::STOPPING => {
                            xr_state.session.end().unwrap();
                            session_running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            break 'main_loop;
                        }
                        _ => {}
                    }
                }
                InstanceLossPending(_) => {
                    break 'main_loop;
                }
                EventsLost(e) => {
                    println!("lost {} events", e.lost_event_count());
                }
                _ => {}
            }
        }

        if !session_running {
            std::thread::sleep(Duration::from_millis(100));
            continue 'main_loop;
        }

        let xr_frame_state = frame_wait.wait().unwrap();
        frame_stream.begin().unwrap();

        xr_state.predicted_display_time = xr_frame_state.predicted_display_time;

        if !xr_frame_state.should_render {
            frame_stream
                .end(
                    xr_frame_state.predicted_display_time,
                    environment_blend_mode,
                    &[],
                )
                .unwrap();
            continue 'main_loop;
        }

        app_state.tasks.retrieve_due(&mut due_tasks);
        while let Some(task) = due_tasks.pop_front() {
            match task {
                TaskType::Global(f) => f(&mut app_state),
                TaskType::Overlay(sel, f) => {
                    if let Some(o) = overlays.mut_by_selector(&sel) {
                        f(&mut app_state, &mut o.state);
                    }
                }
                TaskType::Toast(t) => {
                    // TODO toasts
                    log::info!("Toast: {} {}", t.title, t.body);
                }
            }
        }

        app_state.input_state.pre_update();
        input_source.update(&xr_state, &mut app_state);
        app_state.input_state.post_update();

        if app_state
            .input_state
            .pointers
            .iter()
            .any(|p| p.now.show_hide && !p.before.show_hide)
        {
            if show_hide_counter.click() {
                overlays.show_hide(&mut app_state);
            }
        }

        watch_fade(&mut app_state, overlays.mut_by_id(watch_id).unwrap());

        overlays
            .iter_mut()
            .for_each(|o| o.after_input(&mut app_state));

        #[cfg(feature = "osc")]
        if let Some(ref mut sender) = osc_sender {
            let _ = sender.send_params(&overlays);
        };

        let (_, views) = xr_state
            .session
            .locate_views(
                VIEW_TYPE,
                xr_frame_state.predicted_display_time,
                &xr_state.stage,
            )
            .unwrap();

        app_state.input_state.hmd = helpers::hmd_pose_from_views(&views);

        overlays
            .iter_mut()
            .for_each(|o| o.state.auto_movement(&mut app_state));

        let lengths_haptics = interact(&mut overlays, &mut app_state);
        for (idx, (len, haptics)) in lengths_haptics.iter().enumerate() {
            lines.draw_from(
                pointer_lines[idx],
                app_state.input_state.pointers[idx].pose,
                *len,
                app_state.input_state.pointers[idx].interaction.mode as usize + 1,
                &app_state.input_state.hmd,
            );
            if let Some(haptics) = haptics {
                input_source.haptics(&xr_state, idx, haptics);
            }
        }

        let watch = overlays.mut_by_id(watch_id).unwrap();
        let watch_transform = watch.state.transform;
        if !watch.state.want_visible {
            watch.state.want_visible = true;
            watch.state.transform = Affine3A::from_scale(Vec3 {
                x: 0.001,
                y: 0.001,
                z: 0.001,
            });
        }

        let mut layers = vec![];
        let mut command_buffer = app_state
            .graphics
            .create_command_buffer(CommandBufferUsage::OneTimeSubmit);

        for o in overlays.iter_mut() {
            if !o.state.want_visible {
                continue;
            }

            if !o.data.init {
                o.init(&mut app_state);
                o.data.init = true;
            }

            o.render(&mut app_state);

            let dist_sq = (app_state.input_state.hmd.translation - o.state.transform.translation)
                .length_squared();

            if !dist_sq.is_normal() {
                continue;
            }

            if let Some(quad) = o.present_xr(&xr_state, &mut command_buffer) {
                layers.push((dist_sq, quad));
            };
        }

        for quad in lines.present_xr(&xr_state, &mut command_buffer) {
            layers.push((0.0, quad));
        }

        command_buffer.build_and_execute_now();

        layers.sort_by(|a, b| b.0.total_cmp(&a.0));

        let frame_ref = layers
            .iter()
            .map(|f| &f.1 as &xr::CompositionLayerBase<xr::Vulkan>)
            .collect::<Vec<_>>();

        frame_stream
            .end(
                xr_state.predicted_display_time,
                environment_blend_mode,
                &frame_ref,
            )
            .unwrap();

        app_state.hid_provider.on_new_frame();

        let watch = overlays.mut_by_id(watch_id).unwrap();
        watch.state.transform = watch_transform;
    }

    Ok(())
}
