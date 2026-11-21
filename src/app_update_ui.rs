use eframe::egui::{self, Context, RichText, Ui};
use flash::app_update::{self, Available, Downloaded};
use std::sync::mpsc::{self, Receiver, TryRecvError};

enum State {
    Checking,
    Current,
    Available(Available),
    Downloading,
    Ready(Downloaded),
    Closing,
    Failed(String),
}

pub struct AppUpdateUi {
    state: State,
    result: Option<Receiver<State>>,
    open: bool,
}

impl AppUpdateUi {
    pub fn new(ctx: &Context) -> Self {
        let mut ui = Self {
            state: State::Checking,
            result: None,
            open: false,
        };
        ui.check(ctx);
        ui
    }

    fn run(&mut self, ctx: &Context, state: State, work: impl FnOnce() -> State + Send + 'static) {
        let (send, receive) = mpsc::channel();
        let ctx = ctx.clone();
        self.state = state;
        self.result = Some(receive);
        std::thread::spawn(move || {
            let _ = send.send(work());
            ctx.request_repaint();
        });
    }

    fn check(&mut self, ctx: &Context) {
        self.run(ctx, State::Checking, || match app_update::check() {
            Ok(Some(update)) => State::Available(update),
            Ok(None) => State::Current,
            Err(err) => State::Failed(err),
        });
    }

    pub fn poll(&mut self) {
        let Some(receive) = &self.result else { return };
        self.state = match receive.try_recv() {
            Ok(state) => state,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                State::Failed("The update task stopped unexpectedly.".into())
            }
        };
        self.result = None;
        // Surface a new version on its own instead of waiting for a sidebar click.
        if matches!(self.state, State::Available(_)) {
            self.open = true;
        }
    }

    pub fn sidebar(&mut self, ui: &mut Ui) {
        let label = match &self.state {
            State::Available(update) => format!("Flash {} available", update.version),
            State::Ready(_) => "Flash update ready".into(),
            State::Downloading => "Downloading Flash update…".into(),
            _ => format!("Flash v{} · App updates", env!("CARGO_PKG_VERSION")),
        };
        if ui.small_button(label).clicked() {
            self.open = true;
        }
    }

    pub fn dialog(&mut self, ctx: &Context, busy: bool) {
        if !self.open {
            return;
        }
        let mut check = false;
        let mut download = None;
        let mut apply = false;
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("flash_app_update")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading("Flash updates");
            ui.label(format!("Current version {}", env!("CARGO_PKG_VERSION")));
            ui.add_space(8.0);
            match &self.state {
                State::Checking | State::Downloading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(if matches!(self.state, State::Checking) {
                            "Checking GitHub…"
                        } else {
                            "Downloading and verifying the update…"
                        });
                    });
                }
                State::Current => {
                    ui.label("Flash is up to date.");
                    check = ui.button("Check for updates").clicked();
                }
                State::Available(update) => {
                    ui.label(RichText::new(format!("Flash {} is available", update.version)).strong());
                    ui.hyperlink_to("Release notes", &update.notes);
                    if update.can_install() {
                        if ui.add(crate::primary("Download update")).clicked() {
                            download = Some(update.clone());
                        }
                    } else {
                        ui.label(update.manual_note());
                    }
                }
                State::Ready(_) => {
                    ui.label("The download is verified. Flash will close to apply the update.");
                    if busy {
                        ui.label("Wait for the current Flash task to finish.");
                    }
                    apply = ui.add_enabled(!busy, crate::primary("Install and close Flash")).clicked();
                }
                State::Closing => {
                    ui.label("Closing Flash…");
                }
                State::Failed(err) => {
                    ui.label(RichText::new(err).color(crate::ERR));
                    check = ui.button("Check again").clicked();
                }
            }
            ui.add_space(8.0);
            close = ui.button("Close").clicked();
        });
        if close || response.should_close() {
            self.open = false;
        }
        if check {
            self.check(ctx);
        }
        if let Some(update) = download {
            self.run(
                ctx,
                State::Downloading,
                move || match app_update::download(&update) {
                    Ok(package) => State::Ready(package),
                    Err(err) => State::Failed(err),
                },
            );
        }
        if apply && let State::Ready(package) = std::mem::replace(&mut self.state, State::Closing) {
            match package.apply() {
                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Err(err) => {
                    self.state = State::Failed(format!("Could not start the installer: {err}"))
                }
            }
        }
    }
}
