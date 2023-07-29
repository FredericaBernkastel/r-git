#![allow(unused_labels)]

use {
  std::{
    mem::MaybeUninit,
    pin::pin,
    sync::{self, Arc, OnceLock, RwLock, atomic::AtomicBool},
    time::Duration,
    thread::{self, JoinHandle}
  },
  futures::StreamExt,
  eframe::{
    egui::{self, Align2, Ui, RichText, KeyboardShortcut, Modifiers, Key},
    Storage
  },
  ringbuf::{Rb, SharedRb},
  anyhow::Result,
  r::{Mailer, Submission},
};

struct RedditWatcher {
  watcher: Arc<r::RedditWatcher>,
  watcher_enabled: Arc<AtomicBool>,
  last_posts: Arc<RwLock<SharedRb<Submission, Vec<MaybeUninit<Submission>>>>>
}

fn main() -> Result<()> {
  env_logger::Builder::from_env(
    env_logger::Env::default()
      .default_filter_or("debug")
  ) .format_timestamp(None)
    .format_module_path(false)
    .format_target(false)
    .init();

  let native_options = eframe::NativeOptions {
    follow_system_theme: false,
    ..Default::default()
  };

  eframe::run_native(
    "reddit_watcher",
    native_options,
    Box::new(|cc| Box::new(GUI::new(cc).unwrap()))
  ).unwrap();
  Ok(())
}

struct GUI {
  state: GUIState,

  reddit_watcher: RedditWatcher,
  mailer: Arc<OnceLock<Mailer>>,

  popup_error: (bool, String),
  popup_settings: bool,

  pupup_gui_settings: bool,
  pupup_gui_inspection: bool,
  pupup_gui_memory: bool,

  texture: Option<egui_extras::RetainedImage>
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GUIState {
  subreddit_filter_enabled: bool,
  subreddit_filter: String,
  title_filter_enabled: bool,
  title_filter: String,
  email_enabled: bool,
  email_address: String,

  reddit_fetch_interval: f64,
  subreddit_fetch_interval: f64,
  email_send_interval: f64,
  email_max_submissions_per_letter: usize,
  email_min_submissions_per_letter: usize,
}

impl Default for GUIState {
  fn default() -> Self {
    Self {
      subreddit_filter_enabled: false,
      subreddit_filter: "AskReddit".to_string(),
      title_filter_enabled: false,
      title_filter: "(?i)what".to_string(),
      email_enabled: false,
      email_address: "email@example.com".to_string(),

      reddit_fetch_interval: 10.0,
      subreddit_fetch_interval: 60.0,
      email_send_interval: 10.0,
      email_max_submissions_per_letter: 200,
      email_min_submissions_per_letter: 1,
    }
  }
}

impl From<GUIState> for r::Settings {
  fn from(value: GUIState) -> Self {
    Self {
      subreddit: value.subreddit_filter_enabled.then_some(value.subreddit_filter.clone()),
      submission_filter_regex: value.title_filter_enabled.then_some(value.title_filter.clone()),
      notify_email: value.email_enabled.then_some(value.email_address.clone()),
      reddit_fetch_interval: Some(value.reddit_fetch_interval),
      subreddit_fetch_interval: Some(value.subreddit_fetch_interval),
      email_send_interval: Some(value.email_send_interval),
      email_max_submissions_per_letter: Some(value.email_max_submissions_per_letter),
      email_min_submissions_per_letter: Some(value.email_min_submissions_per_letter),
    }
  }
}

impl GUI {
  fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
    let gui_state = cc.storage
      .and_then(|s| s.get_string("state"))
      .and_then(|json| serde_json::from_str::<GUIState>(&json).ok())
      .unwrap_or_default();

    let reddit_watcher = Arc::new(r::RedditWatcher::new(gui_state.clone().into())?);
    let last_posts = Arc::new(RwLock::new(ringbuf::HeapRb::new(200)));
    let watcher_enabled = Arc::new(AtomicBool::new(false));

    let mailer = Arc::new(OnceLock ::new());

    Mailer::new(gui_state.clone().into())
      .map(|m| mailer.set(m).ok().unwrap())
      .ok();
    Mailer::start_thread(mailer.clone());

    let _watcher_thread = submissions_thread(
      reddit_watcher.clone(),
      last_posts.clone(),
      watcher_enabled.clone(),
      {
        let ctx = cc.egui_ctx.clone();
        move |_| ctx.request_repaint()
      },
        mailer.clone()
    );

    let reddit_watcher = RedditWatcher {
      watcher: reddit_watcher,
      watcher_enabled,
      last_posts
    };

    let texture = egui_extras::RetainedImage::from_image_bytes("rika", include_bytes!("../img/rika.png"))
      .ok();

    Ok(Self {
      reddit_watcher,
      mailer,

      state: gui_state,

      popup_error: (false, "".to_string()),
      popup_settings: false,

      pupup_gui_settings: false,
      pupup_gui_inspection: false,
      pupup_gui_memory: false,

      texture
    })
  }

  fn format_posts(&self, ui: &mut Ui) {
    self.reddit_watcher.last_posts
      .read()
      .map(|lock| {
        lock.iter()
          .rev()
          .for_each(|s| {
            ui.horizontal(|ui| {
              ui.label(RichText::new(format!("[{}] ", s.created_utc)).monospace().small());
              ui.label(RichText::new(format!("{} :", s.subreddit_name_prefixed)).weak().italics());
              ui.hyperlink_to(format!("\"{}\"", s.title), format!("https://reddit.com{}", &s.permalink));
              ui.label(RichText::new(format!(" by {}", s.author)).weak().italics());
            });
          })
      }).ok();
  }

  fn show_error(&mut self, message: String) {
    self.popup_error = (true, message)
  }

  fn on_start_click(&mut self) {
    self.reddit_watcher.watcher_enabled.store(!self.reddit_watcher.watcher_enabled.load(sync::atomic::Ordering::Relaxed), sync::atomic::Ordering::Relaxed);
  }

  fn on_subreddit_filter_changed(&mut self) {
    self.reddit_watcher.watcher
      .with_subredit_filter(self.state.subreddit_filter_enabled.then_some(self.state.subreddit_filter.clone()));
  }

  fn on_title_filter_changed(&mut self) {
    self.reddit_watcher.watcher
      .with_title_filter(self.state.title_filter_enabled.then(||
        regex::Regex::new(&self.state.title_filter)
          .map_err(|e| log::error!("Failed to comile regex: {e:?}"))
          .ok()
      ).flatten())
  }

  fn on_email_checkbox_changed(&mut self) {
    match self.state.email_enabled {
      false => {
        self.mailer.get().map(|m| {
          m.env_settings.write().unwrap().notify_email = None;
        });
      },
      true => if self.mailer.get().is_none() {
        let mailer = Mailer::new(self.state.clone().into());

        match mailer {
          Ok(m) => {
            self.mailer.set(m).ok();
          },
          Err(e) => {
            self.state.email_enabled = false;
            self.show_error(e.to_string());
          }
        };
      } else {
        self.on_email_address_changed();
      }
    };
  }

  fn on_email_address_changed(&mut self) {
    self.mailer.get().map(|m| {
      *m.env_settings.write().unwrap() = self.state.clone().into();
    });
  }

  fn window_error(&mut self, ctx: &egui::Context) {
    egui::Window::new(RichText::new("⚠ Error").color(egui::Color32::from_rgb(255, 192, 0)))
      .open(&mut self.popup_error.0)
      .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
      .resizable(false)
      .show(ctx, |ui| {
        ui.label(RichText::new(&self.popup_error.1).monospace());
      });
  }

  fn window_extra_settings(&mut self, ctx: &egui::Context) {
    egui::Window::new("🔧 GUI Settings")
      .open(&mut self.pupup_gui_settings)
      .vscroll(true)
      .show(ctx, |ui| {
        ctx.settings_ui(ui);
      });

    egui::Window::new("🔍 GUI Inspection")
      .open(&mut self.pupup_gui_inspection)
      .vscroll(true)
      .show(ctx, |ui| {
        ctx.inspection_ui(ui);
      });

    egui::Window::new("📝 GUI Memory")
      .open(&mut self.pupup_gui_memory)
      .resizable(false)
      .show(ctx, |ui| {
        ctx.memory_ui(ui);
      });

    egui::Window::new(RichText::new("🔧 Extra settings"))
      .open(&mut self.popup_settings)
      .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
      .default_width(270.0)
      .show(ctx, |ui| {
        egui::Grid::new("my_grid")
          .num_columns(2)
          .spacing([40.0, 4.0])
          .striped(true)
          .show(ui, |ui| {
            (
              ui.label("reddit_fetch_interval") |
              ui.add(egui::DragValue::new(&mut self.state.reddit_fetch_interval).speed(0.1)
                .suffix("s")
                .clamp_range(1.0..=60.0)
              )
            ) .on_hover_text_at_pointer("Interval to fetch new posts.\nDefault: 10s")
              .changed()
              .then(|| self.reddit_watcher.watcher.with_reddit_fetch_interval(Duration::from_secs_f64(self.state.reddit_fetch_interval)));
            ui.end_row();

            (
              ui.label("subreddit_fetch_interval") |
              ui.add(egui::DragValue::new(&mut self.state.subreddit_fetch_interval).speed(0.5)
                .suffix("s")
                .clamp_range(1.0..=300.0)
              )
            ) .on_hover_text_at_pointer("Interval to fetch new posts, if subreddit filter enabled.\nDefault: 60s")
              .changed()
              .then(|| {
                self.reddit_watcher.watcher.with_subreddit_fetch_interval(Duration::from_secs_f64(self.state.subreddit_fetch_interval));
              });
            ui.end_row();

            ui.separator();
            ui.end_row();

            (
              ui.label("email_send_interval") |
              ui.add(egui::DragValue::new(&mut self.state.email_send_interval).speed(0.5)
                .suffix("m")
                .clamp_range(1.0..=300.0)
              )
            ) .on_hover_text_at_pointer("Send a email at least once per interval.\nDefault: 10 minutes")
              .changed()
              .then(|| {
                self.mailer.get()
                  .map(|m| m.env_settings.write().unwrap().email_send_interval
                    = Some(self.state.email_send_interval));
              });
            ui.end_row();

            (
              ui.label("email_max_submissions_per_letter") |
              ui.add(egui::DragValue::new(&mut self.state.email_max_submissions_per_letter).speed(0.1)
                .clamp_range(1..=1000)
              )
            ) .on_hover_text_at_pointer("Maximum number of submissions per letter.\nDefault: 200")
              .changed()
              .then(|| {
                self.mailer.get()
                  .map(|m| m.env_settings.write().unwrap().email_max_submissions_per_letter
                    = Some(self.state.email_max_submissions_per_letter));
              });
            ui.end_row();

            (
              ui.label("email_min_submissions_per_letter") |
              ui.add(egui::DragValue::new(&mut self.state.email_min_submissions_per_letter).speed(0.1)
                .clamp_range(1..=1000)
              )
            ) .on_hover_text_at_pointer("Only send a letter if above or equal this threshold.\nDefault: 1")
              .changed()
              .then(|| {
                self.mailer.get()
                  .map(|m| m.env_settings.write().unwrap().email_min_submissions_per_letter
                    = Some(self.state.email_min_submissions_per_letter));
              });
            ui.end_row();
          });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(10.0);

        ui.checkbox(&mut self.pupup_gui_settings, "🔧 GUI Settings");
        ui.checkbox(&mut self.pupup_gui_inspection, "🔍 GUI Inspection");
        ui.checkbox(&mut self.pupup_gui_memory, "📝 GUI Memory");
      });
  }
}

impl eframe::App for GUI {
  fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
    // show popups
    self.window_error(ctx);
    self.window_extra_settings(ctx);

    egui::TopBottomPanel::top("control buttons").show(ctx, |ui| {
      ui.add_space(1.0);

      ui.horizontal_wrapped(|ui| {
        ui.style_mut().visuals.button_frame = false;

        (
          ui.button(if !self.reddit_watcher.watcher_enabled.load(sync::atomic::Ordering::Relaxed) { "▶ Start Watcher" } else { "⏸ Stop Watcher" })
            .on_hover_text_at_pointer("S")
            .clicked() || ui.input_mut(|i| i.consume_shortcut(&KeyboardShortcut { modifiers: Modifiers::NONE, key: Key::Space }))
        ).then(|| self.on_start_click());

        ui.label("|");
      });
    });

    egui::SidePanel::left("settings").show(ctx, |ui| {
      ui.add_space(10.0);

      // subreddit filter
      ui.checkbox(&mut self.state.subreddit_filter_enabled, "Subreddit")
        .changed()
        .then(|| self.on_subreddit_filter_changed());
      ui.add_enabled_ui(
        self.state.subreddit_filter_enabled,
        |ui| ui.text_edit_singleline(&mut self.state.subreddit_filter)
          .changed()
          .then(|| self.on_subreddit_filter_changed())
      );

      ui.add_space(5.0);

      // title filter
      ui.checkbox(&mut self.state.title_filter_enabled, "Title filter")
        .changed()
        .then(|| self.on_title_filter_changed());
      ui.add_enabled_ui(
        self.state.title_filter_enabled,
        |ui| ui.text_edit_singleline(&mut self.state.title_filter)
          .changed()
          .then(|| self.on_title_filter_changed())
      );

      ui.add_space(10.0);
      ui.separator();
      ui.add_space(10.0);

      // email
      ui.checkbox(&mut self.state.email_enabled, "✉ Send to email")
        .changed()
        .then(|| self.on_email_checkbox_changed());
      ui.add_enabled_ui(
        self.state.email_enabled,
        |ui| ui.text_edit_singleline(&mut self.state.email_address)
          .changed()
          .then(|| self.on_email_address_changed())
      );

      ui.add_space(10.0);
      ui.separator();
      ui.add_space(10.0);

      ui.checkbox(&mut self.popup_settings, "🔧 Extra settings");

      ui.add_space(80.0);
      self.texture.as_ref().map(|tex| {
        ui.vertical_centered(|ui| {
          ui.image(tex.texture_id(ctx), [128.0, 128.0])
        })
      });
    });

    egui::CentralPanel::default().show(ctx, |ui| {
      egui::containers::ScrollArea::both().show(ui, |ui| {
        self.format_posts(ui);
      });
    });
  }

  fn save(&mut self, storage: &mut dyn Storage) {
    let Ok(json) = serde_json::to_string_pretty(&self.state) else { return };
    storage.set_string("state", json);
  }
}

pub fn submissions_thread(
  reddit_watcher: Arc<r::RedditWatcher>,
  post_ring_buffer: Arc<RwLock<SharedRb<Submission, Vec<MaybeUninit<Submission>>>>>,
  watcher_enabled: Arc<AtomicBool>,
  on_new_post_callback: impl Fn(Submission) + Send + 'static,
  mailer: Arc<OnceLock<Mailer>>
) -> JoinHandle<()> {
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .unwrap();

  thread::spawn(move || {
    runtime.block_on(async move {
      'command: loop {
        if watcher_enabled.load(sync::atomic::Ordering::Relaxed) { // receive enable signal
          let mut stream = pin!(reddit_watcher.stream_submissions().await);
          'stream: while let Some(submission) = stream.next().await {
            if !watcher_enabled.load(sync::atomic::Ordering::Relaxed) { // receive disable signal
              break 'stream;
            };
            post_ring_buffer.write()
              .unwrap()
              .push_overwrite(submission.clone());

            mailer.get()
              .map(|m| {
                m.env_settings.read().unwrap().notify_email
                  .is_some().then(|| {
                  m.add_submission_to_queue(submission.clone())
                })
              });

            tokio::time::sleep(Duration::from_millis(16)).await; // smooth scroll, can be removed
            on_new_post_callback(submission); // call ui thread to redraw post list
          };
        } else {
          tokio::time::sleep(Duration::from_millis(250)).await;
        }
      }
    });
  })
}