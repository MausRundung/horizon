//! The loading screen while the resources load (`Loading`), the window's app before the game
//! takes over (`App`), and the checks a start makes first: missing or damaged prepared files,
//! an unclean last exit, a fullscreen the window system could not show.
use super::*;

/// The window system ended the event loop with a failure (on Linux, a compositor dropping the
/// connection after a protocol error) while fullscreen was on: the next start opens windowed, so a
/// fullscreen the system can't show doesn't crash every launch (#2904).
pub(super) fn leave_fullscreen_after(
    result: &std::result::Result<(), EventLoopError>,
    settings: &mut Settings,
) {
    if matches!(result, Err(EventLoopError::ExitFailure(_))) && settings.fullscreen {
        settings.fullscreen = false;
        settings.save(&Game::options_path());
        log::warn!(
            "the window system closed the game in fullscreen; the next start opens windowed"
        );
    }
}

pub(super) fn run_event_loop(mut game: Game<'_>) -> Result<()> {
    if game.options.headless {
        // No display to open winit's loop on: the game's own loop.
        game.run_headless();
        // narrator::start ran for it too (options.txt's narrator setting).
        narrator::stop();
        let result = write_report(&game).and_then(|()| game.error.take().map_or(Ok(()), Err));
        if game.options.is_client_start() {
            game.native_client_stopped();
            game.native_game_shutting_down();
        }
        return result;
    }
    let mut split = split::Split::new(game);
    let result = EventLoop::new()?.run_app(&mut split);
    game = split.host;
    narrator::stop();
    leave_fullscreen_after(&result, &mut game.settings);
    let result = result
        .map_err(Into::into)
        .and_then(|()| write_report(&game))
        .and_then(|()| game.error.take().map_or(Ok(()), Err));
    if game.options.is_client_start() {
        game.native_client_stopped();
        game.native_game_shutting_down();
    }
    result
}

/// The loading screen's state while the worker loads the resources.
pub(super) struct Loading {
    pub(super) window: Option<Arc<Window>>,
    pub(super) renderer: Option<Renderer>,
    /// The boot screen's atlas, until the pack's GUI atlas replaces it.
    pub(super) boot: horizon_client::hud::HudAtlas,
    pub(super) gui: Option<UiResources>,
    pub(super) settings: Settings,
    pub(super) fullscreen: horizon_client::window::FullscreenState,
    pub(super) video_modes: horizon_client::window::VideoModeList,
    pub(super) options: Option<Options>,
    pub(super) vanilla_root: PathBuf,
    pub(super) pack_error: Option<String>,
    pub(super) started: Instant,
    pub(super) panorama_rotation: horizon_client::gfx::panorama::Rotation,
    pub(super) progress: Progress,
    pub(super) receiver: Receiver<LoaderMsg>,
    pub(super) loaded: Option<std::result::Result<Loaded, String>>,
    /// Item glint textures decoded beside the loading screen, once the device's texture limit is known.
    pub(super) glint_loader: Option<std::thread::JoinHandle<std::result::Result<(), String>>>,
    /// The world renderer prepared offscreen while the loading screen keeps drawing.
    pub(super) renderer_loader: Option<std::thread::JoinHandle<std::result::Result<Renderer, String>>>,
    /// The audio device and the menu's language, prepared beside the renderer.
    pub(super) startup_services:
        Option<std::thread::JoinHandle<(Audio, horizon_client::lang::Language)>>,
    /// The loader's `Done` arrived: its channel closing afterwards is expected.
    pub(super) delivered: bool,
    pub(super) handle: Option<JoinHandle<()>>,
    pub(super) shown: bool,
    /// When the menu bundle arrived (`set_ui`), and whether a title frame with
    /// it was presented since: the world renderer waits for that frame
    /// (`ready_for_handoff`).
    pub(super) ui_at: Option<Instant>,
    pub(super) title_drawn: bool,
    pub(super) menu: startup_menu::StartupMenu,
    pub(super) error: Option<Box<dyn std::error::Error>>,
}
/// How long the handoff waits for the first title frame before building the
/// world renderer anyway (a hidden window may never be asked to redraw).
pub(super) const TITLE_FRAME_WAIT: Duration = Duration::from_secs(1);
impl Loading {
    /// The world renderer is built on the main thread, so building it before
    /// the title is on the screen holds the title back by that build. Wait for
    /// one title frame first, unless a world is waiting to open.
    pub(super) fn ready_for_handoff(&self) -> bool {
        self.title_drawn
            || self.menu.pending.is_some()
            || self.ui_at.is_none_or(|at| at.elapsed() >= TITLE_FRAME_WAIT)
    }
    pub(super) fn glint_ready(&self) -> bool {
        self.glint_loader
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
    pub(super) fn renderer_ready(&self) -> bool {
        self.renderer_loader
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
    }
    pub(super) fn start_renderer_loader(&mut self) -> Result<()> {
        if self.renderer_loader.is_some()
            || self.menu.reload_packs
            || !self.glint_ready()
            || !self.ready_for_handoff()
        {
            return Ok(());
        }
        let loaded = match self.loaded.take() {
            Some(loaded) => loaded,
            None => return Ok(()),
        };
        let mut loaded = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.loaded = Some(Err(error));
                return Ok(());
            }
        };
        let Some(window) = self.window.clone() else {
            self.loaded = Some(Ok(loaded));
            return Ok(());
        };
        let Some(gui) = &self.gui else {
            self.loaded = Some(Ok(loaded));
            return Ok(());
        };
        let renderer_gui = UiResources {
            atlas: gui.atlas.renderer_only(),
            panorama: gui.panorama.clone(),
            splashes: gui.splashes.clone(),
        };
        let pack = loaded.pack.clone();
        let prepared = loaded.renderer_cpu.take();
        let settings = self.settings.clone();
        let display = self
            .renderer
            .as_ref()
            .map(|renderer| renderer.display_reading())
            .unwrap_or_default();
        // The loading renderer already presents to the window: the prepared one picks the
        // same swapchain format and HDR frame.
        let window_output = self
            .renderer
            .as_ref()
            .map(horizon_client::gfx::Renderer::window_output);
        // The sound registry (and device) and the menu's language are read off the same
        // resources the renderer builds from; preparing them here keeps that work out of
        // the handoff's frame.
        let source = loaded.pack.source.clone();
        let sound_device = self.settings.sound_device.clone();
        let audio_on = self
            .options
            .as_ref()
            .is_some_and(|options| options.execution_role.is_client());
        let code = horizon_client::lang::code();
        match std::thread::Builder::new()
            .name("Startup services".into())
            .spawn(move || {
                let _step = startup::step("handoff.services");
                let audio = if audio_on {
                    Audio::with_device(&source, &sound_device)
                } else {
                    Audio::silent(&source)
                };
                let language = horizon_client::lang::Language::load(&[&source], &code);
                (audio, language)
            }) {
            Ok(loader) => self.startup_services = Some(loader),
            Err(error) => log::warn!("Horizon: startup services worker could not start: {error}"),
        }
        let spawned = horizon_guest_stack::builder()
            .name("World renderer".into())
            .spawn(move || {
                build_renderer(
                    window,
                    true,
                    window_output,
                    &pack,
                    &renderer_gui,
                    None,
                    prepared,
                    settings.horizon.effective_api(),
                    settings.horizon.translucent_batching,
                    settings.vsync,
                    &settings.horizon,
                    shader_packs::picked_pack_shape(&settings),
                    display,
                    (
                        settings.file.int("textureFiltering"),
                        settings.file.int("maxAnisotropyBit"),
                    ),
                )
                .map_err(|error| error.to_string())
            });
        self.loaded = Some(Ok(loaded));
        match spawned {
            Ok(loader) => self.renderer_loader = Some(loader),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    pub(super) fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // The Window constructor: created fullscreen when options.txt or --fullscreen says so.
        let fullscreen =
            self.settings.fullscreen || self.options.as_ref().is_some_and(|o| o.fullscreen);
        let size = self
            .options
            .as_ref()
            .map_or([1280, 720], |o| o.window_size)
            .map(f64::from);
        self.fullscreen = horizon_client::window::FullscreenState::new(fullscreen);
        let attributes =
            horizon_client::window::with_icon(Window::default_attributes().with_title("Horizon"));
        // Wayland names the window by its app id, which the horizon.desktop entry matches.
        #[cfg(target_os = "linux")]
        let attributes = winit::platform::wayland::WindowAttributesExtWayland::with_name(
            attributes, "horizon", "horizon",
        );
        // `--offscreen` renders into a texture: its window would only show white.
        let window_step = startup::step("window.create");
        let window = match event_loop.create_window(
            attributes
                .with_inner_size(winit::dpi::LogicalSize::new(size[0], size[1]))
                .with_fullscreen(fullscreen.then_some(winit::window::Fullscreen::Borderless(None)))
                .with_visible(!self.options.as_ref().is_some_and(|o| o.offscreen)),
        ) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.error = Some(e.into());
                event_loop.exit();
                return;
            }
        };
        horizon_client::window::settle_created_fullscreen(&window, fullscreen);
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = hwnd(&window) {
            horizon_threads::system_menu::suppress(hwnd);
        }
        drop(window_step);
        // The renderer takes seconds to build and winit's window sits white
        // until the first frame presents, so paint the boot screen on it and
        // keep repainting it as the console's lines arrive; the wgpu frames
        // redraw the same design when they start. GDI is unsafe:
        // horizon-threads holds it (this crate forbids unsafe code).
        #[cfg(target_os = "windows")]
        // In the fonts given to `menu_style`; with none, the window stays as it is.
        let boot_paint_stop = hwnd(&window)
            .zip(horizon_client::menu_style::get().fonts)
            .map(|(hwnd, fonts)| {
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let repaint_stop = stop.clone();
                std::thread::spawn(move || {
                    while !repaint_stop.load(std::sync::atomic::Ordering::Relaxed) {
                        horizon_threads::boot_paint::paint(
                            hwnd,
                            fonts.semibold,
                            fonts.mono,
                            &horizon_client::logging::recent_lines(),
                        );
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                });
                stop
            });
        let offscreen = self.options.as_ref().is_some_and(|o| o.offscreen);
        let boot_step = startup::step("renderer.boot");
        match loading_renderer(
            window.clone(),
            offscreen,
            Some(&self.boot),
            None,
            None,
            self.settings.horizon.effective_api(),
            shader_packs::picked_pack_shape(&self.settings).ray_tracing,
        ) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                #[cfg(target_os = "windows")]
                if let Some(stop) = &boot_paint_stop {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                self.error = Some(e);
                event_loop.exit();
                return;
            }
        }
        drop(boot_step);
        // The wgpu frames take the window over: stop the boot paint thread
        // before the first one presents.
        #[cfg(target_os = "windows")]
        if let Some(stop) = &boot_paint_stop {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        self.window = Some(window);
    }
    pub(super) fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: WindowId,
        mut event: WindowEvent,
    ) {
        if self.window.as_ref().is_none_or(|w| w.id() != id) {
            return;
        }
        follow_keymap(&mut event);
        if let Err(error) = self.menu_event(event_loop, event) {
            self.error = Some(error);
            event_loop.exit();
        }
    }
    /// The menu bundle arrived: rebuild the loading screen with the real atlas
    /// and title panorama.
    pub(super) fn set_ui(&mut self, gui: UiResources, root: PathBuf) {
        log::info!(
            "Horizon: loading the title artwork after {} ms",
            self.started.elapsed().as_millis()
        );
        let additional = self
            .options
            .as_ref()
            .map_or_else(Vec::new, |options| pack_finders::data_packs(&options.mods));
        self.menu
            .set_root(root, &self.settings, additional.into(), &gui.splashes);
        self.gui = Some(gui);
        self.title_drawn = false;
        horizon_client::ui_theme::reset_atlas();
        self.apply_menu_settings();
        let Some(window) = self.window.clone() else {
            return;
        };
        let offscreen = self.options.as_ref().is_some_and(|o| o.offscreen);
        if let Some(renderer) = &mut self.renderer {
            renderer.release_surface();
        }
        let bundle = self.gui.as_ref().expect("the menu bundle was just set");
        let _step = startup::step("renderer.title_artwork");
        match loading_renderer(
            window,
            offscreen,
            Some(&bundle.atlas),
            Some(&bundle.panorama),
            self.renderer.as_ref(),
            self.settings.horizon.effective_api(),
            shader_packs::picked_pack_shape(&self.settings).ray_tracing,
        ) {
            Ok(r) => {
                self.renderer = Some(r);
                // The GPU holds the atlas now: keep its copy compressed until the world
                // renderer reads it at the handoff, as the game does after its first frame.
                if let Some(gui) = &mut self.gui {
                    gui.atlas.pack_pixels();
                }
            }
            Err(e) => self.error = Some(e),
        }
        // The title frame's wait starts once its renderer is built.
        self.ui_at = Some(Instant::now());
    }
    pub(super) fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The loop may run once more after exit(): keep the first error, the
        // one that ended loading.
        if self.error.is_some() {
            event_loop.exit();
            return;
        }
        if horizon_threads::stop_requests() > 0 {
            // No world is open yet: nothing to save.
            event_loop.exit();
            return;
        }
        if let Err(error) = self.poll_menu_resources() {
            self.error = Some(error);
            event_loop.exit();
            return;
        }
        self.poll_menu_realms(event_loop);
        if let Some(w) = &self.window {
            if self.options.as_ref().is_some_and(|o| o.offscreen) {
                self.draw();
            } else {
                w.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::Poll);
    }
    pub(super) fn draw(&mut self) {
        let Some(size) = self
            .renderer
            .as_ref()
            .map(|r| [r.config.width, r.config.height])
        else {
            return;
        };
        let mut batch = HudBatch::new(size, self.settings.gui_scale_inputs());
        self.draw_menu(&mut batch);
        self.menu.frame_drawn((batch.width, batch.height));
        let mut frame = loading_frame(&batch);
        frame.yaw = self.panorama_rotation.update(
            self.started.elapsed().as_secs_f32(),
            self.settings.file.double("panoramaScrollSpeed") as f32,
        );
        if let Some(renderer) = &mut self.renderer {
            match renderer.render(&frame) {
                Ok(true) if self.gui.is_some() && self.menu.pending.is_none() => {
                    self.title_drawn = true;
                }
                Ok(_) => {}
                Err(e) => log::warn!("Horizon: loading screen frame failed: {e}"),
            }
        }
        startup::counters("frame", &[]);
        if !self.shown {
            self.shown = true;
            startup::mark("loading_screen.shown");
            log::info!(
                "Horizon: loading screen shown after {} ms",
                self.started.elapsed().as_millis()
            );
        }
    }
    pub(super) fn finish(&mut self, mut loaded: Loaded) -> Result<Option<split::Split<'static>>> {
        let handoff_at = Instant::now();
        let joins = startup::step("handoff.joins");
        if let Some(loader) = self.glint_loader.take() {
            loader
                .join()
                .map_err(|_| "item glint atlas loader panicked")?
                .map_err(std::io::Error::other)?;
        }
        let mut renderer = self
            .renderer_loader
            .take()
            .ok_or("world renderer was not prepared")?
            .join()
            .map_err(|_| "world renderer loader panicked")?
            .map_err(std::io::Error::other)?;
        drop(joins);
        let gui = self.gui.take().ok_or("missing menu bundle")?;
        // The renderer was built from a copy, so its reset of the font cache did not reach
        // this one: glyphs the loading screen's texture held are placed into the world
        // renderer's atlas texture again when the game draws them.
        let font_reset = startup::step("handoff.font_reset");
        gui.atlas.font.reset_cache();
        drop(font_reset);
        let mut options = self.options.take().ok_or("loading already finished")?;
        let window = self.window.take().ok_or("missing window")?;
        options.resources = loaded.root.clone();
        // StartupMenu owns world selection until it can hand it to Game.
        options.world = None;
        options.server = None;
        let mut previous = self.renderer.take();
        let release = startup::step("handoff.release");
        if let Some(renderer) = &mut previous {
            renderer.release_surface();
            if let Some(atlas) = &renderer.hud.atlas_texture {
                atlas.destroy();
            }
        }
        drop(release);
        // Tearing down the loading screen's device (and its pipelines) can take the driver a
        // while; it happens on a thread of its own, beside the attach.
        if let Some(previous) = previous.take()
            && let Err(error) = std::thread::Builder::new()
                .name("Old renderer".into())
                .spawn(move || drop(previous))
        {
            log::warn!("Horizon: the old renderer could not be freed beside the handoff: {error}");
        }
        let renderer_at = Instant::now();
        if !options.offscreen {
            let _attach_step = startup::step("renderer.attach_window");
            renderer.attach_window()?;
        }
        log::info!(
            "Horizon: world renderer attached after {} ms",
            renderer_at.elapsed().as_millis()
        );
        let renderer_ms = renderer_at.elapsed().as_millis();
        let (audio, language) = match self.startup_services.take() {
            Some(loader) => {
                let _step = startup::step("handoff.services_join");
                loader
                    .join()
                    .map_err(|_| "startup services worker panicked")?
            }
            None => {
                let audio = if !options.execution_role.is_client() {
                    Audio::silent(&loaded.pack.source)
                } else {
                    Audio::with_device(&loaded.pack.source, &self.settings.sound_device)
                };
                // The menu read its language from the prepared root alone (`set_root`) and
                // `load_resources` skips it on this path: read it again with the mods' layers,
                // in the language picked in the menu.
                let language = horizon_client::lang::Language::load(
                    &[&loaded.pack.source],
                    &horizon_client::lang::code(),
                );
                (audio, language)
            }
        };
        let lang = startup::step("handoff.lang");
        horizon_client::lang::set(language);
        drop(lang);
        let pack_error = loaded.pack_error.or_else(|| self.pack_error.take());
        // The game borrows its shared resources for the process's lifetime,
        // like the single copy an integrated server would share.
        let res: &'static Arc<Resources> = Box::leak(Box::new(loaded.resources));
        let game_at = Instant::now();
        let game_step = startup::step("handoff.start_game");
        let Some(mut game) = start_game(
            res,
            loaded.pack,
            gui,
            options,
            pack_error,
            audio,
            self.vanilla_root.clone(),
            self.started,
            loaded.rest_loader,
            Some(self.settings.clone()),
            loaded.game_cpu,
            std::mem::take(&mut loaded.client_applies),
            loaded.entity_models,
        )?
        else {
            return Ok(None);
        };
        drop(game_step);
        let game_ms = game_at.elapsed().as_millis();
        game.renderer = Some(renderer);
        game.resize_state = Some(game.resize_state_of(window.inner_size()));
        game.window = Some(window);
        game.world_renderer_ready = true;
        game.fullscreen = self.fullscreen;
        game.panorama_rotation = self.panorama_rotation;
        self.menu.handoff(&mut game);
        game.apply_settings();
        game.previous = Instant::now();
        // Minecraft.onGameLoadFinished: a start that made it this far is clean.
        game.settings.file.set(
            "startedCleanly",
            horizon_client::options_file::V::Bool(true),
        );
        game.settings.save(&Game::options_path());
        log::info!(
            "Horizon: startup handoff {} ms (renderer {} ms, start_game {} ms)",
            handoff_at.elapsed().as_millis(),
            renderer_ms,
            game_ms
        );
        Ok(Some(split::Split::new(game)))
    }
}

/// The application: the loading screen until the resources are ready, then
/// the game itself, on the same event loop and window.
pub(super) enum App {
    Loading(Box<Loading>),
    Ready(Box<split::Split<'static>>),
}
impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match self {
            App::Loading(loading) => loading.resumed(event_loop),
            App::Ready(game) => game.resumed(event_loop),
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match self {
            App::Loading(loading) => loading.window_event(event_loop, id, event),
            App::Ready(game) => game.window_event(event_loop, id, event),
        }
    }
    fn device_event(&mut self, event_loop: &ActiveEventLoop, id: DeviceId, event: DeviceEvent) {
        if let App::Ready(game) = self {
            game.device_event(event_loop, id, event);
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        enum Step {
            Waiting,
            Ready(Box<split::Split<'static>>),
            Done,
            Failed(Box<dyn std::error::Error>),
        }
        let step = match self {
            App::Ready(game) => {
                game.about_to_wait(event_loop);
                return;
            }
            App::Loading(loading) => {
                loading.about_to_wait(event_loop);
                if loading.error.is_some() {
                    event_loop.exit();
                    return;
                }
                if let Err(error) = loading.start_renderer_loader() {
                    loading.error = Some(error);
                    event_loop.exit();
                    return;
                }
                match if loading.menu.reload_packs
                    || !loading.ready_for_handoff()
                    || !loading.glint_ready()
                    || !loading.renderer_ready()
                {
                    None
                } else {
                    loading.loaded.take()
                } {
                    None => Step::Waiting,
                    Some(Err(e)) => Step::Failed(e.into()),
                    Some(Ok(loaded)) => match loading.finish(loaded) {
                        Ok(Some(game)) => Step::Ready(Box::new(game)),
                        Ok(None) => Step::Done,
                        Err(e) => Step::Failed(e),
                    },
                }
            }
        };
        match step {
            Step::Waiting => {}
            Step::Ready(game) => *self = App::Ready(game),
            Step::Done => event_loop.exit(),
            Step::Failed(e) => {
                if let App::Loading(loading) = self {
                    loading.error = Some(e);
                }
                event_loop.exit();
            }
        }
    }
    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        if let App::Ready(game) = self {
            game.exiting(event_loop);
        }
    }
}

/// Every launch reads the prepared game files (`horizon-prepare`'s output, whose
/// manifest is `horizon-resources.json`): without them, say where they were
/// looked for and how to get them, before a window or a loader starts. `get` is
/// the page a hook sends the player to (`horizon_hooks::engine::get_the_game`).
pub(super) fn missing_resources(
    root: &std::path::Path,
    get: Option<horizon_hooks::GetTheGame>,
) -> Option<String> {
    let shown = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    if !root.join("horizon-resources.json").is_file() {
        let launcher = get.map_or(String::new(), |get| {
            format!(
                "Horizon is started from its launcher: download it at {} (Download). It needs \
                 your own Minecraft: Java Edition; the launcher signs you in and prepares the \
                 game files.\nDevelopers: ",
                get.address
            )
        });
        return Some(format!(
            "Horizon's prepared game files are missing: no horizon-resources.json in {}\n\
             {launcher}horizon-prepare --prepare, or pass --resources <folder>",
            shown.display(),
        ));
    }
    damaged_prepared(root).map(|problem| {
        format!(
            "Horizon's prepared game files are damaged: {problem} in {}\n\
             Prepare them again with the launcher, or: horizon-prepare --prepare",
            shown.display()
        )
    })
}

/// Which top-level prepared file a killed preparation or an interrupted copy
/// left unusable. The loader parses these before it can start; naming one
/// beats serde's bare "expected value at line 1 column 1".
pub(super) fn damaged_prepared(root: &std::path::Path) -> Option<String> {
    let manifest = root.join("horizon-resources.json");
    let bytes = std::fs::read(&manifest).ok()?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Some("horizon-resources.json is empty".into());
    }
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        return Some("horizon-resources.json is not valid JSON".into());
    }
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() == 0) {
            return Some(format!("{} is empty", entry.file_name().to_string_lossy()));
        }
    }
    None
}

/// `Minecraft.<init>`: `startedCleanly` left false means the previous start
/// died before `onGameLoadFinished`. Vanilla then leaves fullscreen
/// ("resetting fullscreen mode") so a start that died on the display mode can
/// come back; the resource packs are rolled back only when their load fails
/// (`rollbackResourcePacks`), not here. True when the last start was unclean.
pub(super) fn unclean_start_recovery(settings: &mut Settings) -> bool {
    // Vanilla's field defaults true: only an explicit `false` is an unclean
    // start; an options file that never had the key is not one.
    let unclean = matches!(
        settings.file.get("startedCleanly"),
        Some(horizon_client::options_file::V::Bool(false))
    );
    if unclean {
        settings.fullscreen = false;
        settings.file.fullscreen_resolution = None;
    }
    unclean
}
