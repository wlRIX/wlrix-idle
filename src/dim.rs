// SPDX-License-Identifier: GPL-3.0-or-later
//! Dimming: a dark layer faded in over every monitor, and on a laptop a lower backlight.
//!
//! # The overlay
//!
//! One `wlr-layer-shell` surface per output, on the overlay layer, anchored to every edge, with an
//! empty input region -- so a click goes straight through it to whatever is underneath, and the
//! input that ends the dim (the compositor sees it, `ext-idle-notify` says `resumed`) is never
//! eaten. Its content is a single black pixel with some alpha, stretched over the output by
//! `wp_viewporter`, so a 4K monitor costs four bytes. The fade is a dozen such pixels, each a
//! little darker, swapped in on a timer; going back is instant, because a person who has just
//! touched the mouse wants to see the screen.
//!
//! An overlay works on any monitor, nested as well, and does not fight a night-light tool for
//! the gamma ramp. What it does not do is save power, which on a laptop is the point of dimming
//! on battery -- hence the backlight.
//!
//! # The backlight
//!
//! On a machine with a panel backlight (`/sys/class/backlight`), dimming also takes it to a
//! third of where it was, through logind as `wlrix-osd`'s brightness keys do, and undimming puts
//! back exactly what was there -- unless somebody changed it meanwhile, which the brightness keys
//! on the way back to the session are: then theirs stands.

use std::time::Duration;

use calloop::RegistrationToken;
use calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::raw::RawPool;
use smithay_client_toolkit::{delegate_layer, delegate_shm};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_output::WlOutput, wl_region::WlRegion, wl_shm,
};
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use crate::backlight::{self, Backlight};
use crate::idle::Idle;

/// How dark the overlay ends, out of 255.
const DARKEST: u8 = 150;
/// The fade: this many steps, this far apart -- about a second in all.
const STEPS: usize = 12;
const STEP: Duration = Duration::from_millis(80);

/// One output's overlay.
struct Overlay {
    output: WlOutput,
    layer: LayerSurface,
    viewport: WpViewport,
    /// The size the compositor gave it; nothing is drawn until it has.
    size: Option<(u32, u32)>,
}

/// What dimming has done, so it can be undone.
#[derive(Default)]
pub struct Dim {
    pub active: bool,
    overlays: Vec<Overlay>,
    /// The fade's pixels, darkest last; made the first time anything dims.
    pixels: Option<(RawPool, Vec<WlBuffer>)>,
    step: usize,
    timer: Option<RegistrationToken>,
    /// The backlight, where it was, and where dimming put it.
    backlight: Option<(Backlight, u32, u32)>,
}

/// Fade every output to dark, and lower the backlight. Does nothing if already dim.
pub fn dim(idle: &mut Idle) {
    if idle.dim.active {
        return;
    }
    idle.dim.active = true;
    idle.dim.step = 0;
    info!("dimming");

    if overlay_available(idle) {
        ensure_pixels(idle);
        for output in idle.outputs() {
            add_overlay(idle, output);
        }
        let token = idle.loop_handle().insert_source(
            Timer::from_duration(STEP),
            |_, _, idle: &mut Idle| {
                if !idle.dim.active || idle.dim.step + 1 >= STEPS {
                    idle.dim.timer = None;
                    return TimeoutAction::Drop;
                }
                idle.dim.step += 1;
                redraw_all(idle);
                TimeoutAction::ToDuration(STEP)
            },
        );
        idle.dim.timer = token.ok();
    }

    if let Some(panel) = Backlight::find()
        && let Some(current) = panel.current()
    {
        let target = backlight::dimmed(current);
        if target < current {
            idle.dim.backlight = Some((panel.clone(), current, target));
            set_backlight(panel, target);
        }
    }
}

/// Undo [`dim`], at once.
pub fn undim(idle: &mut Idle) {
    if !idle.dim.active {
        return;
    }
    idle.dim.active = false;
    if let Some(token) = idle.dim.timer.take() {
        idle.loop_handle().remove(token);
    }
    for overlay in idle.dim.overlays.drain(..) {
        overlay.viewport.destroy();
        // Dropping the layer surface destroys it.
        drop(overlay.layer);
    }
    if let Some((panel, before, dimmed)) = idle.dim.backlight.take() {
        // Someone moved it while it was dim -- the brightness keys, on the way back: theirs wins.
        if panel.current() == Some(dimmed) {
            set_backlight(panel, before);
        }
    }
    info!("undimmed");
}

/// An output appeared while dim: dim it too.
pub fn dim_new_output(idle: &mut Idle, output: WlOutput) {
    if idle.dim.active && overlay_available(idle) {
        add_overlay(idle, output);
    }
}

/// An output went away; its overlay went with it.
pub fn forget_output(idle: &mut Idle, output: &WlOutput) {
    idle.dim
        .overlays
        .retain(|overlay| &overlay.output != output);
}

fn overlay_available(idle: &Idle) -> bool {
    let available = idle.layer_shell.is_some() && idle.viewporter.is_some();
    if !available {
        warn!("no layer shell or viewporter, so dimming only lowers the backlight");
    }
    available
}

/// The fade's pixels: premultiplied black, each a little more opaque than the last.
fn ensure_pixels(idle: &mut Idle) {
    if idle.dim.pixels.is_some() {
        return;
    }
    let mut pool = match RawPool::new(STEPS * 4, &idle.shm) {
        Ok(pool) => pool,
        Err(err) => {
            warn!("could not make the dim overlay's pixels: {err}");
            return;
        }
    };
    let qh = idle.qh.clone();
    let mut buffers = Vec::with_capacity(STEPS);
    for step in 0..STEPS {
        let alpha = (DARKEST as usize * (step + 1) / STEPS) as u8;
        // ARGB8888 is little-endian B, G, R, A; black premultiplied is all zero but the alpha.
        pool.mmap()[step * 4..step * 4 + 4].copy_from_slice(&[0, 0, 0, alpha]);
        buffers.push(pool.create_buffer(
            (step * 4) as i32,
            1,
            1,
            4,
            wl_shm::Format::Argb8888,
            (),
            &qh,
        ));
    }
    idle.dim.pixels = Some((pool, buffers));
}

fn add_overlay(idle: &mut Idle, output: WlOutput) {
    let (Some(layer_shell), Some(viewporter), Some(compositor)) =
        (&idle.layer_shell, &idle.viewporter, &idle.compositor)
    else {
        return;
    };
    let qh = idle.qh.clone();
    let surface = compositor.create_surface(&qh);
    // Empty: every click and every motion goes through to what is underneath.
    let region: WlRegion = compositor.wl_compositor().create_region(&qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();
    let viewport = viewporter.get_viewport(&surface, &qh, ());
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("wlrix-idle-dim"),
        Some(&output),
    );
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(0, 0);
    layer.commit();
    idle.dim.overlays.push(Overlay {
        output,
        layer,
        viewport,
        size: None,
    });
}

fn redraw_all(idle: &mut Idle) {
    let Some((_, buffers)) = &idle.dim.pixels else {
        return;
    };
    let Some(buffer) = buffers.get(idle.dim.step) else {
        return;
    };
    for overlay in idle
        .dim
        .overlays
        .iter()
        .filter(|overlay| overlay.size.is_some())
    {
        draw(overlay, buffer);
    }
}

fn draw(overlay: &Overlay, buffer: &WlBuffer) {
    let Some((width, height)) = overlay.size else {
        return;
    };
    overlay
        .viewport
        .set_destination(width as i32, height as i32);
    let surface = overlay.layer.wl_surface();
    surface.attach(Some(buffer), 0, 0);
    surface.damage_buffer(0, 0, 1, 1);
    surface.commit();
}

/// Set the backlight off the loop: logind answers quickly, but the loop owes nobody a wait.
fn set_backlight(panel: Backlight, value: u32) {
    let spawned = std::thread::Builder::new()
        .name("wlrix-idle-backlight".into())
        .spawn(move || {
            let result =
                zbus::blocking::Connection::system().and_then(|system| panel.set(&system, value));
            if let Err(err) = result {
                warn!("could not set the backlight: {err}");
            }
        });
    if let Err(err) = spawned {
        warn!("could not start the backlight thread: {err}");
    }
}

impl LayerShellHandler for Idle {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        self.dim.overlays.retain(|overlay| &overlay.layer != layer);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let step = self.dim.step;
        let Some(buffer) = self
            .dim
            .pixels
            .as_ref()
            .and_then(|(_, buffers)| buffers.get(step).cloned())
        else {
            return;
        };
        if let Some(overlay) = self
            .dim
            .overlays
            .iter_mut()
            .find(|overlay| &overlay.layer == layer)
        {
            let (width, height) = configure.new_size;
            overlay.size = Some((width.max(1), height.max(1)));
            draw(overlay, &buffer);
        }
    }
}

impl smithay_client_toolkit::shm::ShmHandler for Idle {
    fn shm_state(&mut self) -> &mut smithay_client_toolkit::shm::Shm {
        &mut self.shm
    }
}

delegate_layer!(Idle);
delegate_shm!(Idle);
// A buffer that is released is simply reused; the region and the viewport objects have no
// events worth hearing.
wayland_client::delegate_noop!(Idle: ignore WlBuffer);
wayland_client::delegate_noop!(Idle: ignore WlRegion);
wayland_client::delegate_noop!(Idle: WpViewporter);
wayland_client::delegate_noop!(Idle: WpViewport);
