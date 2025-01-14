use std::error::Error;
use std::ffi::CString;
use std::num::NonZeroU32;

use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::event_loop::EventLoop;
use winit::window::{self, CursorGrabMode, Icon, WindowAttributes};

use glutin::config::{Config, ConfigTemplateBuilder, GetGlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentContext, PossiblyCurrentContext, Version,
};
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin::surface::{Surface, SwapInterval, WindowSurface};

use glutin_winit::{DisplayBuilder, GlWindow};

pub use glutin::display::GlDisplay;
pub use winit::event;
pub use winit::keyboard;

pub mod gl {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));

    pub use Gles2 as Gl;
}

impl<S, H: AppEventHandler<AppState = S>, R: AppRenderer<AppState = S>> ApplicationHandler
    for App<S, H, R>
{
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (window, gl_config) = match &self.gl_display {
            // We just created the event loop, so initialize the display, pick the config, and
            // create the context.
            GlDisplayCreationState::Builder(display_builder) => {
                let (window, gl_config) = match display_builder.clone().build(
                    event_loop,
                    self.template.clone(),
                    gl_config_picker,
                ) {
                    Ok((window, gl_config)) => {
                        let window = window.unwrap();
                        window.set_cursor_visible(self.window_info.cursor_visible);
                        if self.window_info.cursor_grabbed {
                            window
                                .set_cursor_grab(CursorGrabMode::Confined)
                                .or_else(|_e| window.set_cursor_grab(CursorGrabMode::Locked))
                                .unwrap();
                        }
                        (window, gl_config)
                    }
                    Err(err) => {
                        self.exit_state = Err(err);
                        event_loop.exit();
                        return;
                    }
                };

                // Mark the display as initialized to not recreate it on resume, since the
                // display is valid until we explicitly destroy it.
                self.gl_display = GlDisplayCreationState::Init;

                // Create gl context.
                self.gl_context =
                    Some(create_gl_context(&window, &gl_config).treat_as_possibly_current());

                (window, gl_config)
            }
            GlDisplayCreationState::Init => {
                println!("Recreating window in `resumed`");
                // Pick the config which we already use for the context.
                let gl_config = self.gl_context.as_ref().unwrap().config();
                match glutin_winit::finalize_window(
                    event_loop,
                    window_attributes(&self.window_info),
                    &gl_config,
                ) {
                    Ok(window) => {
                        window.set_cursor_visible(self.window_info.cursor_visible);
                        if self.window_info.cursor_grabbed {
                            window
                                .set_cursor_grab(CursorGrabMode::Confined)
                                .or_else(|_e| window.set_cursor_grab(CursorGrabMode::Locked))
                                .unwrap();
                        }
                        (window, gl_config)
                    }
                    Err(err) => {
                        self.exit_state = Err(err.into());
                        event_loop.exit();
                        return;
                    }
                }
            }
        };

        let attrs = window
            .build_surface_attributes(Default::default())
            .expect("Failed to build surface attributes");
        let gl_surface = unsafe {
            gl_config
                .display()
                .create_window_surface(&gl_config, &attrs)
                .unwrap()
        };

        // The context needs to be current for the Renderer to set up shaders and
        // buffers. It also performs function loading, which needs a current context on
        // WGL.
        let gl_context = self.gl_context.as_ref().unwrap();
        gl_context.make_current(&gl_surface).unwrap();

        self.renderer.get_or_insert_with(|| {
            let gl = gl::Gl::load_with(|symbol| {
                let symbol = CString::new(symbol).unwrap();
                gl_config
                    .display()
                    .get_proc_address(symbol.as_c_str())
                    .cast()
            });
            R::new(gl)
        });

        // Try setting vsync.
        if let Err(res) = gl_surface
            .set_swap_interval(gl_context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()))
        {
            eprintln!("Error setting vsync: {res:?}");
        }

        assert!(self
            .gl_state
            .replace(GlState { gl_surface, window })
            .is_none());
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // This event is only raised on Android, where the backing NativeWindow for a GL
        // Surface can appear and disappear at any moment.
        println!("Android window removed");

        // Destroy the GL Surface and un-current the GL Context before ndk-glue releases
        // the window back to the system.
        self.gl_state = None;

        // Make context not current.
        self.gl_context = Some(
            self.gl_context
                .take()
                .unwrap()
                .make_not_current()
                .unwrap()
                .treat_as_possibly_current(),
        );
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::Resized(size) if size.width != 0 && size.height != 0 => {
                // Some platforms like EGL require resizing GL surface to update the size
                // Notable platforms here are Wayland and macOS, other don't require it
                // and the function is no-op, but it's wise to resize it for portability
                // reasons.
                if let Some(GlState {
                    gl_surface,
                    window: _,
                }) = self.gl_state.as_ref()
                {
                    let gl_context = self.gl_context.as_ref().unwrap();
                    gl_surface.resize(
                        gl_context,
                        NonZeroU32::new(size.width).unwrap(),
                        NonZeroU32::new(size.height).unwrap(),
                    );

                    let renderer: &mut R = self.renderer.as_mut().unwrap();
                    renderer.resize(size.width as i32, size.height as i32);
                }
            }
            event => match self.handler.handle_event(&mut self.app_state, event) {
                Ok(AppControl::Continue) => (),
                Ok(AppControl::Exit) => event_loop.exit(),
                Err(e) => {
                    self.exit_state = Err(e);
                    event_loop.exit();
                }
            },
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // NOTE: The handling below is only needed due to nvidia on Wayland to not crash
        // on exit due to nvidia driver touching the Wayland display from on
        // `exit` hook.
        let _gl_display = self.gl_context.take().unwrap().display();

        // Clear the window.
        self.gl_state = None;
        #[cfg(egl_backend)]
        #[allow(irrefutable_let_patterns)]
        if let glutin::display::Display::Egl(display) = _gl_display {
            unsafe {
                display.terminate();
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(GlState { gl_surface, window }) = self.gl_state.as_ref() {
            let gl_context = self.gl_context.as_ref().unwrap();
            let renderer = self.renderer.as_ref().unwrap();
            renderer.draw(&mut self.app_state);
            window.request_redraw();

            gl_surface.swap_buffers(gl_context).unwrap();
        }
    }
}

fn create_gl_context(window: &window::Window, gl_config: &Config) -> NotCurrentContext {
    let raw_window_handle = window.window_handle().ok().map(|wh| wh.as_raw());

    // The context creation part.
    let context_attributes = ContextAttributesBuilder::new().build(raw_window_handle);

    // Since glutin by default tries to create OpenGL core context, which may not be
    // present we should try gles.
    let fallback_context_attributes = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::Gles(None))
        .build(raw_window_handle);

    // There are also some old devices that support neither modern OpenGL nor GLES.
    // To support these we can try and create a 2.1 context.
    let legacy_context_attributes = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::OpenGl(Some(Version::new(2, 1))))
        .build(raw_window_handle);

    // Reuse the uncurrented context from a suspended() call if it exists, otherwise
    // this is the first time resumed() is called, where the context still
    // has to be created.
    let gl_display = gl_config.display();

    unsafe {
        gl_display
            .create_context(gl_config, &context_attributes)
            .unwrap_or_else(|_| {
                gl_display
                    .create_context(gl_config, &fallback_context_attributes)
                    .unwrap_or_else(|_| {
                        gl_display
                            .create_context(gl_config, &legacy_context_attributes)
                            .expect("failed to create context")
                    })
            })
    }
}

fn window_attributes(window_info: &WindowInformation) -> WindowAttributes {
    let mut attr = window::Window::default_attributes()
        .with_fullscreen(if window_info.fullscreen {
            Some(window::Fullscreen::Borderless(None))
        } else {
            None
        })
        .with_resizable(window_info.resizable)
        .with_transparent(window_info.transparent)
        .with_title(&window_info.title)
        .with_window_icon(window_info.icon.clone());

    if let Some((x, y)) = window_info.size {
        attr = attr.with_inner_size(PhysicalSize::new(x as u32, y as u32));
    }

    attr
}

enum GlDisplayCreationState {
    /// The display was not build yet.
    Builder(DisplayBuilder),
    /// The display was already created for the application.
    Init,
}

struct App<S, H: AppEventHandler<AppState = S>, R: AppRenderer<AppState = S>> {
    template: ConfigTemplateBuilder,
    renderer: Option<R>,
    app_state: S,
    handler: H,
    window_info: WindowInformation,
    // NOTE: `GlState` carries the `Window`, thus it should be dropped after everything else.
    gl_state: Option<GlState>,
    gl_context: Option<PossiblyCurrentContext>,
    gl_display: GlDisplayCreationState,
    exit_state: Result<(), Box<dyn Error>>,
}

impl<S, H: AppEventHandler<AppState = S>, R: AppRenderer<AppState = S>> App<S, H, R> {
    fn new(
        template: ConfigTemplateBuilder,
        window_info: WindowInformation,
        display_builder: DisplayBuilder,
        app_state: S,
        handler: H,
    ) -> Self {
        Self {
            template,
            app_state,
            handler,
            window_info,
            renderer: None,
            gl_display: GlDisplayCreationState::Builder(display_builder),
            gl_context: None,
            gl_state: None,
            exit_state: Ok(()),
        }
    }
}

struct GlState {
    gl_surface: Surface<WindowSurface>,
    // NOTE: Window should be dropped after all resources created using its
    // raw-window-handle.
    window: window::Window,
}

// Find the config with the maximum number of samples, so our triangle will be
// smooth.
pub fn gl_config_picker(configs: Box<dyn Iterator<Item = Config> + '_>) -> Config {
    configs
        .reduce(|accum, config| {
            let transparency_check = config.supports_transparency().unwrap_or(false)
                & !accum.supports_transparency().unwrap_or(false);

            if transparency_check || config.num_samples() > accum.num_samples() {
                config
            } else {
                accum
            }
        })
        .unwrap()
}

pub trait AppRenderer {
    type AppState;

    fn new(gl: gl::Gl) -> Self;
    fn draw(&self, app_state: &mut Self::AppState);
    fn resize(&mut self, _width: i32, _height: i32) {}
}

pub enum AppControl {
    Continue,
    Exit,
}

pub trait AppEventHandler {
    type AppState;
    fn handle_event(
        &mut self,
        app_state: &mut Self::AppState,
        event: WindowEvent,
    ) -> Result<AppControl, Box<dyn Error>>;
}

impl<S> AppEventHandler for fn(&mut S, WindowEvent) -> Result<AppControl, Box<dyn Error>> {
    type AppState = S;
    fn handle_event(
        &mut self,
        app_state: &mut Self::AppState,
        event: WindowEvent,
    ) -> Result<AppControl, Box<dyn Error>> {
        self(app_state, event)
    }
}

pub type HandleFn<S> = for<'a> fn(
    &'a mut S,
    WindowEvent,
) -> Result<AppControl, Box<(dyn std::error::Error + 'static)>>;

struct WindowInformation {
    pub transparent: bool,
    pub fullscreen: bool,
    pub resizable: bool,
    pub size: Option<(usize, usize)>,
    pub title: String,
    pub icon: Option<Icon>,
    pub cursor_visible: bool,
    pub cursor_grabbed: bool,
}

pub struct Window<S, H: AppEventHandler<AppState = S>, R: AppRenderer<AppState = S>> {
    window_info: WindowInformation,
    _s: std::marker::PhantomData<S>,
    _h: std::marker::PhantomData<H>,
    _r: std::marker::PhantomData<R>,
}

impl<S, H: AppEventHandler<AppState = S>, R: AppRenderer<AppState = S>> Window<S, H, R> {
    pub fn new() -> Window<S, H, R> {
        Window {
            window_info: WindowInformation {
                transparent: true,
                fullscreen: false,
                resizable: true,
                size: None,
                title: "".to_string(),
                icon: None,
                cursor_visible: true,
                cursor_grabbed: false,
            },
            _s: std::marker::PhantomData,
            _h: std::marker::PhantomData,
            _r: std::marker::PhantomData,
        }
    }

    pub fn set_transparent(mut self, transparent: bool) -> Window<S, H, R> {
        self.window_info.transparent = transparent;
        self
    }

    pub fn set_fullscreen(mut self, fullscreen: bool) -> Window<S, H, R> {
        self.window_info.fullscreen = fullscreen;
        self
    }

    pub fn set_resizable(mut self, resizable: bool) -> Window<S, H, R> {
        self.window_info.resizable = resizable;
        self
    }

    pub fn set_size(mut self, size: (usize, usize)) -> Window<S, H, R> {
        self.window_info.size = Some(size);
        self
    }

    pub fn set_title(mut self, title: &str) -> Window<S, H, R> {
        self.window_info.title = title.to_string();
        self
    }

    pub fn set_icon(mut self, data: &[u8], width: usize, height: usize) -> Window<S, H, R> {
        self.window_info.icon =
            Some(Icon::from_rgba(data.to_vec(), width as u32, height as u32).unwrap());
        self
    }

    pub fn set_cursor_visible(mut self, visible: bool) -> Window<S, H, R> {
        self.window_info.cursor_visible = visible;
        self
    }

    pub fn set_cursor_grabbed(mut self, grabbed: bool) -> Window<S, H, R> {
        self.window_info.cursor_grabbed = grabbed;
        self
    }

    pub fn run(self, state: S, handler: H) -> Result<(), Box<dyn Error>> {
        let event_loop = EventLoop::new().unwrap();

        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_transparency(cfg!(cgl_backend));

        let display_builder = DisplayBuilder::new()
            .with_window_attributes(Some(window_attributes(&self.window_info)));

        let mut app =
            App::<S, H, R>::new(template, self.window_info, display_builder, state, handler);
        event_loop.run_app(&mut app)?;

        app.exit_state
    }
}

impl<S, H: AppEventHandler<AppState = S>, R: AppRenderer<AppState = S>> Default
    for Window<S, H, R>
{
    fn default() -> Self {
        Self::new()
    }
}
