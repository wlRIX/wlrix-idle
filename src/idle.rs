// SPDX-License-Identifier: GPL-3.0-or-later
//! The state machine, and the loop it runs on.
//!
//! Everything is a source on one calloop event loop: the Wayland connection, the D-Bus threads'
//! channel, the inotify watch on `/dev/input` and each controller's fd. Nothing polls and
//! nothing sleeps, so the whole program is idle while the session is not.
//!
//! ## Three verbs
//!
//! [`Idle::arm_all`] destroys every `ext_idle_notification_v1` and creates a fresh one per
//! configured timeout. Because the compositor starts counting the moment a notification is
//! created, that single act means "restart every countdown from the full timeout" -- which is
//! how an inhibitor being released, a controller being touched and a config reload all end.
//!
//! [`Idle::went_idle`] runs one timeout's actions. [`Idle::unwind`] undoes them.
//!
//! ## Why the resume actions are not driven by `resumed`
//!
//! It would be natural to run a timeout's `resume_command` when its notification says
//! `resumed`. It cannot work: a notification destroyed while it is idle -- which is exactly
//! what an inhibitor arriving does -- will never send `resumed` at all, and the dim would
//! never be undone. So the unwinding is driven from our own record of what fired, and
//! `resumed` is only one of the four things that trigger it.

use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::{
    delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::calloop_wayland_source::WaylandSource,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output::WlOutput, wl_seat::WlSeat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::ExtIdleNotificationV1, ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1;

use crate::{
    Args, action, config,
    config::{Config, Timeout},
    dbus, gamepad, notify, outputs,
    outputs::Control,
    pidfile, signals,
};

/// One configured timeout, and what has become of it.
struct Step {
    timeout: Timeout,
    /// The live protocol object. `None` while inhibited, and between a reload's teardown and
    /// its rebuild.
    notification: Option<ExtIdleNotificationV1>,
    /// This timeout's actions have run and have not yet been undone.
    fired: bool,
}

pub struct Idle {
    registry_state: RegistryState,
    output_state: OutputState,
    pub qh: QueueHandle<Idle>,
    pub conn: Connection,
    loop_handle: LoopHandle<'static, Idle>,

    notifier: ExtIdleNotifierV1,
    pub power_manager: Option<ZwlrOutputPowerManagerV1>,
    seat: WlSeat,
    /// Outputs we are holding switched off. Empty whenever the screens are on.
    pub controls: Vec<Control>,

    pub config: Config,
    config_path: Option<PathBuf>,
    steps: Vec<Step>,

    /// Something has asked the session to stay awake. Countdowns do not run while this is set.
    inhibited: bool,
    /// We switched the monitors off and owe the user a screen.
    pub blanked: bool,
    /// The locker, while it is running. A second timeout asking to lock is a no-op.
    lock_child: Option<Child>,
    /// Commands started by a timeout, kept only so they can be reaped.
    children: Vec<Child>,

    dbus: Option<dbus::Handle>,
    pub pads: gamepad::Pads,
    exit: bool,
}

impl Idle {
    /// Every output the compositor has told us about.
    pub fn outputs(&self) -> Vec<WlOutput> {
        self.output_state.outputs().collect()
    }

    pub fn loop_handle(&self) -> &LoopHandle<'static, Idle> {
        &self.loop_handle
    }

    /// Rebuild the step list from the config. Leaves the notifications alone; the caller arms.
    fn rebuild_steps(&mut self) {
        self.steps = self
            .config
            .timeouts
            .iter()
            .cloned()
            .map(|timeout| Step {
                timeout,
                notification: None,
                fired: false,
            })
            .collect();
    }

    /// Start every countdown from the full timeout.
    ///
    /// Destroying and recreating rather than trying to reset: the protocol has no reset, and
    /// creating a notification is defined to start counting immediately, so this *is* the
    /// reset. It is also the only thing that restarts a notification that has already gone
    /// idle, since such an object will never fire again on its own.
    pub fn arm_all(&mut self) {
        self.disarm_all();
        if self.inhibited {
            return;
        }
        let notifier = self.notifier.clone();
        let seat = self.seat.clone();
        let qh = self.qh.clone();
        for (index, step) in self.steps.iter_mut().enumerate() {
            let millis = Duration::from_secs(step.timeout.after_secs).as_millis();
            let millis = millis.min(u32::MAX as u128) as u32;
            step.notification = Some(notifier.get_idle_notification(millis, &seat, &qh, index));
        }
    }

    /// Stop every countdown, leaving what has already fired alone.
    fn disarm_all(&mut self) {
        for step in &mut self.steps {
            if let Some(notification) = step.notification.take() {
                notification.destroy();
            }
        }
    }

    /// One countdown ran out.
    pub fn went_idle(&mut self, index: usize) {
        let Some(step) = self.steps.get(index) else {
            return;
        };
        if step.fired {
            return;
        }
        let after = step.timeout.after_secs;
        let (blank, lock, command) = (
            step.timeout.blank,
            step.timeout.lock,
            step.timeout.command.clone(),
        );
        self.steps[index].fired = true;
        info!("idle for {after}s");

        // Lock before blank, deliberately. The locker has to be up and covering the screen
        // before the monitors go dark, or a screen that comes back early -- a stray input
        // event, a monitor that renegotiates its link -- shows the session behind it.
        if lock {
            self.lock();
        }
        if blank {
            outputs::blank(self);
        }
        if let Some(command) = command
            && let Some(child) = action::spawn(&command)
        {
            self.children.push(child);
        }
        self.publish_active();
    }

    /// Tell the D-Bus side whether the screensaver is, as far as anyone asking is concerned,
    /// showing. That is exactly "a timeout has fired and has not been undone".
    fn publish_active(&mut self) {
        let Some(dbus) = &self.dbus else {
            return;
        };
        dbus.set_active(self.blanked || self.steps.iter().any(|step| step.fired));
    }

    /// Undo whatever the timeouts did. Idempotent, and safe to call when nothing has fired.
    ///
    /// Does not touch the notifications: which of the four callers needs the countdowns
    /// restarted differs, so that is the caller's business.
    pub fn unwind(&mut self, why: &str) {
        let anything = self.blanked || self.steps.iter().any(|step| step.fired);
        if !anything {
            return;
        }
        info!("awake ({why})");

        // First, and before any command runs. The screen coming back is what the user is
        // standing there waiting for; a slow `sh -c` must not be in front of it.
        outputs::unblank(self);

        // Deepest first: the last thing that happened is the first thing undone.
        let resumes: Vec<String> = self
            .steps
            .iter()
            .rev()
            .filter(|step| step.fired)
            .filter_map(|step| step.timeout.resume_command.clone())
            .collect();
        for step in &mut self.steps {
            step.fired = false;
        }
        for command in resumes {
            if let Some(child) = action::spawn(&command) {
                self.children.push(child);
            }
        }

        // The locker is deliberately left running. It ends when the user authenticates to it,
        // not when they touch the mouse -- killing it here would mean any passer-by could
        // unlock the session by moving it.

        self.publish_active();
    }

    /// The user did something the compositor did not see -- a controller, or an application
    /// asking on their behalf. Undo the timeouts and start counting again from scratch.
    pub fn activity(&mut self, why: &str) {
        self.unwind(why);
        self.arm_all();
    }

    /// Something asked the session to stay awake, or stopped asking.
    fn set_inhibited(&mut self, inhibited: bool, detail: &str) {
        if self.inhibited == inhibited {
            return;
        }
        self.inhibited = inhibited;
        if inhibited {
            info!("holding off: {detail}");
            // Undo first. An application saying "not now" while the screen is already off means
            // it wants the screen, not merely that it wants the countdown paused.
            self.unwind("inhibited");
            self.disarm_all();
        } else {
            info!("nothing is holding off any more; counting again");
            self.arm_all();
        }
    }

    /// Start the locker, unless it is already running.
    fn lock(&mut self) {
        if let Some(child) = &mut self.lock_child
            && matches!(child.try_wait(), Ok(None))
        {
            return;
        }
        let Some(command) = self.config.lock.command.clone() else {
            warn!("something asked to lock, but [lock] command is not set");
            return;
        };
        info!("locking");
        self.lock_child = action::spawn(&command);
    }

    /// A message from one of the D-Bus threads.
    fn on_dbus(&mut self, event: dbus::Event) {
        match event {
            dbus::Event::Inhibited { active, detail } => self.set_inhibited(active, &detail),
            dbus::Event::Activity => self.activity("asked over D-Bus"),
            dbus::Event::Lock => self.lock(),
            dbus::Event::BeforeSleep => self.before_sleep(),
            dbus::Event::AfterSleep => self.after_sleep(),
            dbus::Event::NameUnavailable { name, owner } => {
                warn!(
                    "{name} is already owned by {owner}; inhibit requests will go there, not here"
                );
            }
        }
    }

    /// The machine is about to suspend, and logind is waiting on us.
    fn before_sleep(&mut self) {
        info!("about to sleep");
        let before = self.config.before_sleep.clone();
        if before.lock {
            self.lock();
        }
        if before.blank {
            outputs::blank(self);
        }
        if let Some(command) = &before.command
            && !command.is_empty()
            && let Some(child) = action::spawn(command)
        {
            self.children.push(child);
        }
        // Everything is started, so let go of the delay inhibitor. The thread holds it until
        // this arrives or its own timeout runs out, whichever comes first.
        if let Some(dbus) = &self.dbus {
            let _ = self.conn.flush();
            dbus.ack_sleep();
        }
    }

    /// Back from suspend.
    ///
    /// Deliberately not an `activity`: the screen is still off and, if it locked, still locked,
    /// and that is correct -- nobody has proved they are there yet. All that is needed is for
    /// the countdowns to run from the moment of waking rather than from before the suspend,
    /// which a monotonic clock that did not advance while the machine slept would otherwise
    /// leave them doing.
    fn after_sleep(&mut self) {
        info!("awake from sleep; counting again");
        self.arm_all();
    }

    /// `SIGHUP`: re-read the config file.
    fn reload(&mut self) {
        // Under the *old* config, so a `resume_command` about to be edited away still gets its
        // chance to undo whatever its timeout did.
        self.unwind("reload");

        let loaded = config::load(self.config_path.as_deref());
        match loaded.source {
            config::Source::Rejected(_) => {
                warn!("keeping the running config");
                self.arm_all();
                return;
            }
            _ => info!("reloaded {}", loaded.source.describe()),
        }

        let gamepad_changed = loaded.config.gamepad != self.config.gamepad;
        let dbus_changed = loaded.config.dbus != self.config.dbus;
        self.config = loaded.config;
        info!("{}", self.config.summary());

        self.rebuild_steps();
        self.arm_all();

        if gamepad_changed {
            gamepad::restart(self);
        }
        if dbus_changed {
            // Dropping and retaking a bus name would silently void every inhibit applications
            // are holding right now, and open a gap for another daemon to take
            // org.freedesktop.ScreenSaver. A reload must not leave things worse than not
            // reloading, so this one needs the deliberate act of a restart.
            warn!("[dbus] changed; that only takes effect on a restart");
        }
    }

    /// Clear out anything that has exited.
    fn reap(&mut self) {
        let children = std::mem::take(&mut self.children);
        self.children = action::reap(children);
        if let Some(child) = &mut self.lock_child
            && !matches!(child.try_wait(), Ok(None))
        {
            self.lock_child = None;
        }
    }
}

/// Connect, wire everything onto one loop, and stay until told to stop.
pub fn run(config: Config, args: &Args) -> Result<(), String> {
    let conn = Connection::connect_to_env()
        .map_err(|err| format!("no Wayland compositor to connect to: {err}"))?;
    let (globals, event_queue) = registry_queue_init::<Idle>(&conn)
        .map_err(|err| format!("could not read the registry: {err}"))?;
    let qh = event_queue.handle();

    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);

    // Required. Without it there is no way to learn that the session went idle, and nothing
    // this program could usefully do instead.
    let notifier = registry_state
        .bind_one::<ExtIdleNotifierV1, _, _>(&qh, notify::VERSION..=notify::VERSION, ())
        .map_err(|err| {
            format!("the compositor does not offer ext-idle-notify-v1 ({err}); nothing to do")
        })?;
    let seat = registry_state
        .bind_one::<WlSeat, _, _>(&qh, 1..=1, ())
        .map_err(|err| format!("no seat to watch ({err})"))?;
    // Optional: a config that only runs commands works without it, so this degrades with a
    // message rather than refusing to start.
    let power_manager = match registry_state.bind_one::<ZwlrOutputPowerManagerV1, _, _>(
        &qh,
        outputs::VERSION..=outputs::VERSION,
        (),
    ) {
        Ok(manager) => Some(manager),
        Err(err) => {
            warn!("no wlr-output-power-management ({err}); `blank` will do nothing");
            None
        }
    };

    let mut event_loop: EventLoop<'static, Idle> =
        EventLoop::try_new().map_err(|err| format!("could not create the event loop: {err}"))?;
    let loop_handle = event_loop.handle();

    let mut idle = Idle {
        registry_state,
        output_state,
        qh: qh.clone(),
        conn: conn.clone(),
        loop_handle: loop_handle.clone(),
        notifier,
        power_manager,
        seat,
        controls: Vec::new(),
        config,
        config_path: args.config.clone(),
        steps: Vec::new(),
        inhibited: false,
        blanked: false,
        lock_child: None,
        children: Vec::new(),
        dbus: None,
        pads: gamepad::Pads::default(),
        exit: false,
    };
    idle.rebuild_steps();

    // Kept for the loop below, to tell a compositor that has gone away from a real failure.
    let health = conn.clone();
    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|err| format!("could not drive Wayland from the loop: {err}"))?;

    let (quit_ping, quit_source) = calloop::ping::make_ping()
        .map_err(|err| format!("could not create the quit ping: {err}"))?;
    loop_handle
        .insert_source(quit_source, |_, _, idle: &mut Idle| idle.exit = true)
        .map_err(|err| format!("could not watch for a stop signal: {err}"))?;
    signals::forward_to_loop(quit_ping);

    let (reload_ping, reload_source) = calloop::ping::make_ping()
        .map_err(|err| format!("could not create the reload ping: {err}"))?;
    loop_handle
        .insert_source(reload_source, |_, _, idle: &mut Idle| idle.reload())
        .map_err(|err| format!("could not watch for SIGHUP: {err}"))?;
    signals::forward_reload_to_loop(reload_ping);

    if !args.no_dbus
        && let Some((handle, channel)) = dbus::spawn(&idle.config, args.replace)
    {
        idle.dbus = Some(handle);
        loop_handle
            .insert_source(channel, |event, _, idle: &mut Idle| {
                if let calloop::channel::Event::Msg(event) = event {
                    idle.on_dbus(event);
                }
            })
            .map_err(|err| format!("could not watch the D-Bus threads: {err}"))?;
    }

    gamepad::start(&mut idle);

    let _pidfile = pidfile::write();

    // The countdowns need a seat and an output list, which arrive from the loop; one dispatch
    // settles both, and `new_output` covers anything that turns up later.
    event_loop
        .dispatch(Duration::from_millis(200), &mut idle)
        .map_err(|err| format!("initial dispatch failed: {err}"))?;
    idle.arm_all();

    let result = loop {
        if let Err(err) = event_loop.dispatch(Duration::from_secs(1), &mut idle) {
            // The compositor going away is the ordinary end of a Wayland client's life, not a
            // failure: logging out looks exactly like this, and a daemon that reports an error
            // every logout teaches people to ignore its output.
            break if health.flush().is_err() {
                Ok(())
            } else {
                Err(format!("event loop failed: {err}"))
            };
        }
        idle.reap();
        if idle.exit {
            info!("stopping");
            break Ok(());
        }
    };

    // Whatever happened, the screens do not stay off. See `outputs::unblank_on_exit`.
    outputs::unblank_on_exit(&mut idle);
    result
}

impl OutputHandler for Idle {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        outputs::blank_new_output(self, output);
    }

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}

    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        outputs::forget_output(self, &output);
    }
}

impl ProvidesRegistryState for Idle {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

wayland_client::delegate_noop!(Idle: ignore WlSeat);
delegate_output!(Idle);
delegate_registry!(Idle);
