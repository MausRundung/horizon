# The startup handoff: from the loading screen to the world renderer

The first playable frame used to build the world renderer on the main thread: device
selection, pipeline and texture creation, and the surface's first configuration all ran after
the loading screen closed. That frame took hundreds of milliseconds on Vulkan and DX12, and
its cost moved with the driver and the shader compiler rather than with the world.

The work now happens beside the loading screen. The loading screen still draws its frames,
the renderer and the start's services are prepared on workers, and the handoff itself is
reduced to attaching a prepared renderer to the window.

## The pipeline today

1. **The loading screen draws.** Every frame writes the startup trace's `frame` counter
   (`loading_screen.rs`, `spans::startup::counters("frame", &[])`), so the loading screen's
   pacing is measured the same way the menu's and the world's are.
2. **The glint atlas starts early.** When the loader delivers `Done`, the device's maximum
   texture size is already known; the pack's `ItemRenderer::prepare_glint_atlas` runs on the
   `Item glint atlas` worker (`startup_menu.rs`). It is memoized by
   `(max_texture_dimension_2d, textureFiltering, maxAnisotropyBit)`, so a reload that does not
   change those limits reuses the atlas.
3. **The world renderer is built on a worker.** `Loading::start_renderer_loader`
   (`loading_screen.rs`) waits for the glints and for `ready_for_handoff`, then builds the
   world renderer on the `World renderer` thread. It is given:
   - the menu bundle's renderer-only copies (`HudAtlas::renderer_only` and
     `Font::renderer_only`), so the menu's textures and caches stay on the main thread;
   - `WindowOutput` read from the renderer that currently presents (`Renderer::window_output`),
     so the prepared renderer configures the same swapchain format, present mode, alpha mode,
     colour space and HDR frame as the surface offers — the loading renderer is the one that
     negotiated them;
   - the CPU-side renderer work the loader prepared (`renderer_cpu`), so the expensive texture
     and model work is not repeated.
4. **Audio and language are prepared in parallel.** A `Startup services` worker creates the
   `Audio` device and loads the menu's `Language` off the main thread. Both are joined at the
   handoff; a run without the worker falls back to creating them inline.
5. **The handoff attaches instead of building.** `Loading::finish`:
   - joins the glint and renderer workers (`handoff.joins`);
   - resets the menu atlas's font cache (`handoff.font_reset`): the world renderer was built
     from a copy, so its cache reset did not reach the menu bundle;
   - releases the loading renderer's surface and HUD atlas and frees the whole loading
     renderer on the `Old renderer` thread (`handoff.release`), so driver teardown does not
     block the attach;
   - calls `Renderer::attach_window`, which only creates, configures and watches the surface
     (`surface.create`, `surface.configure`, `surface.watch`);
   - joins the services and installs the language (`handoff.services_join`, `handoff.lang`).

   The log line is now `world renderer attached after N ms`, and the first game frame logs
   `first frame after N ms (encode N ms, wait N ms)`, with `first_frame` counters carrying the
   same two numbers into the startup trace.

A pack reload while the loading screen runs drops the workers it started
(`startup_menu.rs`), because a renderer or service set built for the replaced pack must not be
handed off.

## Banded texture uploads no longer wait

`gfx/upload.rs` stages large textures a band (`BAND`, 8 MiB) at a time. Each band is now
recorded through the same `Uploads` staging ring the frame's uploads use, submitted, and the
device is *polled* rather than waited on. A slow upload therefore cannot freeze the render
thread at a band boundary; the previous band is retired by the ring when its buffer is reused.

## Measuring it

The startup trace records the steps, the counters and one event per rendered frame:

```sh
HORIZON_STARTUP_TRACE=$TMPDIR/startup.jsonl ./horizon-client
python3 tools/startup_trace.py $TMPDIR/startup.jsonl
```

- `handoff.*`, `surface.*`, `renderer.attach_window` and `renderer.glint_atlas` are the steps.
- `frame` counters mark every rendered frame on every screen; gaps between them are the
  loading screen's, the menu's and the world's hitches.
- `first_frame` carries `encode_ms` and `wait_ms` for the first game frame.
- Marks (`loading_screen.shown`, `title.shown`, `world.opening`, `world.first_frame`,
  `world.view_full`) bracket the phases.

Representative numbers from the test machine (Ryzen 7 2700X, Radeon RX Vega): the attach is
about 14 ms on DX12 and 142 ms on Vulkan (82 ms configuring the surface, 60 ms watching it),
where the whole handoff used to be a single 210–270 ms frame on Vulkan; a first game frame is
about 57 ms in the renderer (51 ms encoding, 3 ms waiting). Treat these as the shape of the
cost, not as targets: driver and shader cache decide them.

For the world's own frames, start a capture (`horizon-ctl profile client start`) and read the
result with `horizon-perf`; the startup trace ends at the menu.
