use std::mem::MaybeUninit;
use std::pin::pin;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use eframe::egui::{Align2, Ui};
use r::{Mailer, Submission};
use ringbuf::{Rb, SharedRb};
use {
  anyhow::Result,
  eframe::egui::{self, Label, RichText, KeyboardShortcut, Modifiers, Key},
  futures::{Stream, StreamExt},
  std::{
    thread::{self, JoinHandle},
  },
};

struct RedditWatcher {
  reddit_watcher: Arc<r::RedditWatcher>,
  thread: JoinHandle<()>,
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
    "My egui App",
    native_options,
    Box::new(|cc| Box::new(GUI::new(cc).unwrap()))
  ).unwrap();
  Ok(())
}

struct GUI {
  reddit_watcher: RedditWatcher,
  mailer: Arc<Option<Mailer>>,

  subreddit_filter_enabled: bool,
  subreddit_filter: String,
  title_filter_enabled: bool,
  title_filter: String,
  email_enabled: bool,
  email_address: String,

  popup_error: (bool, String)
}

impl GUI {
  fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
    // Customize egui here with cc.egui_ctx.set_fonts and cc.egui_ctx.set_visuals.
    // Restore app state using cc.storage (requires the "persistence" feature).
    // Use the cc.gl (a glow::Context) to create graphics shaders and buffers that you can use
    // for e.g. egui::PaintCallback.

    let reddit_watcher = Arc::new(r::RedditWatcher::new()?);
    let last_posts = Arc::new(RwLock::new(ringbuf::HeapRb::new(200)));
    let ctx = cc.egui_ctx.clone();
    let watcher_enabled = Arc::new(AtomicBool::new(false));

    let mailer = Arc::new(Mailer::new(r::Settings {
      subreddit: None,
      submission_filter_regex: None,
      notify_email: None,
    }).ok());
    Mailer::start_thread(mailer.clone());

    let thread = submissions_thread(
      reddit_watcher.clone(),
      last_posts.clone(),
      watcher_enabled.clone(),
      move |_| ctx.request_repaint(),
        mailer.clone()
    );

    let reddit_watcher = RedditWatcher {
      reddit_watcher,
      thread,
      watcher_enabled,
      last_posts
    };

    Ok(Self {
      reddit_watcher,
      mailer,

      subreddit_filter_enabled: false,
      subreddit_filter: "AskReddit".to_string(),
      title_filter_enabled: false,
      title_filter: "(?i)what".to_string(),
      email_enabled: false,
      email_address: "email@example.com".to_string(),

      popup_error: (false, "".to_string())
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

  fn show_error_popup(&mut self, message: String) {
    self.popup_error = (true, message)
  }

  fn on_start_click(&mut self) {
    self.reddit_watcher.watcher_enabled.store(!self.reddit_watcher.watcher_enabled.load(Ordering::Relaxed), Ordering::Relaxed);
  }

  fn on_subreddit_filter_changed(&mut self) {
    self.reddit_watcher.reddit_watcher
      .with_subredit_filter(self.subreddit_filter_enabled.then_some(self.subreddit_filter.clone()));
  }

  fn on_title_filter_changed(&mut self) {
    self.reddit_watcher.reddit_watcher
      .with_title_filter(self.title_filter_enabled.then(||
        regex::Regex::new(&self.title_filter)
          .map_err(|e| log::error!("Failed to comile regex: {e:?}"))
          .ok()
      ).flatten())
  }

  fn on_email_checkbox_changed(&mut self) {
    match self.email_enabled {
      false => {
        (*self.mailer).as_ref().map(|m| {
          m.env_settings.write().unwrap().notify_email = None;
        });
      },
      true => if self.mailer.is_none() {
        let mailer = r::Mailer::new(r::Settings {
          subreddit: self.subreddit_filter_enabled.then_some(self.subreddit_filter.clone()),
          submission_filter_regex: self.title_filter_enabled.then_some(self.title_filter.clone()),
          notify_email: Some(self.email_address.clone()),
        });

        match mailer {
          Ok(m) => {
            self.mailer = Arc::new(Some(m));
            let thr = Mailer::start_thread(self.mailer.clone());
          },
          Err(e) => {
            self.email_enabled = false;
            self.show_error_popup(e.to_string());
          }
        };
      } else {
        self.on_email_address_changed();
      }
    };
  }

  fn on_email_address_changed(&mut self) {
    (*self.mailer).as_ref().map(|m| {
      let env_settings = r::Settings {
        subreddit: self.subreddit_filter_enabled.then_some(self.subreddit_filter.clone()),
        submission_filter_regex: self.title_filter_enabled.then_some(self.title_filter.clone()),
        notify_email: Some(self.email_address.clone()),
      };
      *m.env_settings.write().unwrap() = env_settings;
    });
  }
}

impl eframe::App for GUI {
  fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
    // show popups
    egui::Window::new(RichText::new("⚠ Error").color(egui::Color32::from_rgb(255, 192, 0)))
      .open(&mut self.popup_error.0)
      .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
      .resizable(false)
      .show(ctx, |ui| {
        ui.label(RichText::new(&self.popup_error.1).monospace());
      });


    egui::TopBottomPanel::top("control buttons").show(ctx, |ui| {
      ui.add_space(1.0);

      ui.horizontal_wrapped(|ui| {
        ui.style_mut().visuals.button_frame = false;

        (
          ui.button(if !self.reddit_watcher.watcher_enabled.load(Ordering::Relaxed) { "▶ Start Watcher" } else { "⏸ Stop Watcher" })
            .on_hover_text_at_pointer("S")
            .clicked() || ui.input_mut(|i| i.consume_shortcut(&KeyboardShortcut { modifiers: Modifiers::NONE, key: Key::Space }))
        ).then(|| self.on_start_click());

        ui.label("|");
      });
    });

    egui::SidePanel::left("settings").show(ctx, |ui| {
      ui.add_space(10.0);

      // subreddit filter
      ui.checkbox(&mut self.subreddit_filter_enabled, "Subreddit")
        .changed()
        .then(|| self.on_subreddit_filter_changed());
      ui.add_enabled_ui(
        self.subreddit_filter_enabled,
        |ui| ui.text_edit_singleline(&mut self.subreddit_filter)
          .changed()
          .then(|| self.on_subreddit_filter_changed())
      );

      ui.add_space(5.0);

      // title filter
      ui.checkbox(&mut self.title_filter_enabled, "Title filter")
        .changed()
        .then(|| self.on_title_filter_changed());
      ui.add_enabled_ui(
        self.title_filter_enabled,
        |ui| ui.text_edit_singleline(&mut self.title_filter)
          .changed()
          .then(|| self.on_title_filter_changed())
      );

      ui.add_space(10.0);
      ui.separator();
      ui.add_space(10.0);

      // email
      ui.checkbox(&mut self.email_enabled, "Send to email")
        .changed()
        .then(|| self.on_email_checkbox_changed());
      ui.add_enabled_ui(
        self.email_enabled,
        |ui| ui.text_edit_singleline(&mut self.email_address)
          .changed()
          .then(|| self.on_email_address_changed())
      );
    });

    egui::CentralPanel::default().show(ctx, |ui| {
      egui::containers::ScrollArea::both().show(ui, |ui| {
        self.format_posts(ui);
      });
    });
  }
}

pub fn submissions_thread(
  reddit_watcher: Arc<r::RedditWatcher>,
  post_ring_buffer: Arc<RwLock<SharedRb<Submission, Vec<MaybeUninit<Submission>>>>>,
  watcher_enabled: Arc<AtomicBool>,
  on_new_post_callback: impl Fn(Submission) + Send + 'static,
  mailer: Arc<Option<Mailer>>
) -> JoinHandle<()> {
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .unwrap();

  thread::spawn(move || {
    runtime.block_on(async move {
      'command: loop {
        if watcher_enabled.load(Ordering::Relaxed) { // receive enable signal
          let mut stream = pin!(reddit_watcher.stream_submissions().await);
          'stream: while let Some(submission) = stream.next().await {
            if !watcher_enabled.load(Ordering::Relaxed) { // receive disable signal
              break 'stream;
            };
            post_ring_buffer.write()
              .unwrap()
              .push_overwrite(submission.clone());

            (*mailer).as_ref()
              .map(|m| m.add_submission_to_queue(submission.clone()));

            tokio::time::sleep(Duration::from_millis(16)).await; // smooth scroll, can be removed
            on_new_post_callback(submission); // call ui thread to redraw post list
          };
        } else {
          thread::sleep(Duration::from_millis(250));
        }
      }
    });
  })
}