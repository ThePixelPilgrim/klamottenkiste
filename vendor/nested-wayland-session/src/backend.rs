//! Backend initialisation: the GLES renderer, the nested output, and the dmabuf
//! protocol state.
//!
//! The default path is HEADLESS (`init_headless_backend`): it opens a DRM render
//! node, builds a headless EGL context from a gbm device, creates a `GlesRenderer`,
//! and renders into an offscreen GLES renderbuffer — no window, no winit. The
//! Output/dmabuf-global setup that follows is shared with the legacy winit path via
//! `finish_backend_setup`. The old winit nested-window entry point (`init_backend`)
//! is preserved behind `#[cfg(feature = "backend_winit")]` but not built by default.

/// What:     Grouped `use` of the headless building blocks: gbm device + Fourcc, the
///           EGL display/context/device, the GLES renderer and its offscreen types,
///           the dmabuf import traits, output types, transform/size geometry, and the
///           dmabuf protocol state.
/// Why:      Everything `init_headless_backend`, `finish_backend_setup`, and
///           `HeadlessBackend` reference.
use smithay::{
    backend::{
        allocator::{
            dmabuf::{AsDmabuf, Dmabuf},
            gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
            Allocator, Buffer as _, Fourcc, Modifier,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            gles::{GlesError, GlesRenderbuffer, GlesRenderer, GlesTarget},
            Bind, ImportDma, ImportEgl, Offscreen,
        },
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::wayland_server::DisplayHandle,
    utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform},
    wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
};

/// What:     `use anyhow::{anyhow, Context, Result};`. Error helpers.
/// Why:      The backend init functions return `Result` and annotate failures.
use anyhow::{anyhow, Context, Result};

/// What:     `use tracing::{info, trace, warn};`. Structured log macros.
/// Why:      Report the chosen render node, dmabuf version, and hardware-acceleration;
///           `trace` keeps the per-release bookkeeping chatter out of a resize storm's log.
use tracing::{info, trace, warn};

/// What:     `use std::{fs::File, os::fd::AsRawFd, path::PathBuf};`. Owned file handle,
///           the raw-fd accessor for a dmabuf plane, and a path.
/// Why:      Opening the DRM render node yields a `File` used as the gbm fd; exporting a
///           dmabuf plane's borrowed fd into the plain `DmabufFrame` needs `AsRawFd`.
use std::{fs::File, os::fd::AsRawFd, path::PathBuf};

/// What:     `use crossbeam_channel::{Receiver, Sender};`. The two halves of the
///           slot-release channel.
/// Why:      The consumer sends a freed pool-slot id; the render loop drains it before the
///           next bind and returns that slot to the free pool.
use crossbeam_channel::{Receiver, Sender};

/// What:     `use crate::state::BackendPieces;`. The carrier struct for the built pieces.
/// Why:      Both init paths return `BackendPieces`.
use crate::state::BackendPieces;

/// Milli-hertz refresh rate reported for the nested output (60.000 Hz).
///
/// What:     `pub const OUTPUT_REFRESH_MHZ: i32 = 60_000;`. Smithay reports refresh in
///           millihertz, so 60 Hz is 60000.
/// Why:      Named so the magic number is not repeated at the mode-construction sites.
pub const OUTPUT_REFRESH_MHZ: i32 = 60_000;

/// The concrete render backend the compositor state carries.
///
/// What:     A type alias that resolves to the headless backend by default, or the
///           winit graphics backend when `backend_winit` is enabled.
/// Why:      Lets `Compositor`/`BackendPieces` name one backend type while the winit
///           path stays a compile-time opt-in.
#[cfg(not(feature = "backend_winit"))]
pub type RenderBackend = HeadlessBackend;

/// What:     `pub type RenderBackend = WinitGraphicsBackend<GlesRenderer>;` (winit build).
/// Why:      Preserve the original backend type when the legacy feature is on.
#[cfg(feature = "backend_winit")]
pub type RenderBackend = smithay::backend::winit::WinitGraphicsBackend<GlesRenderer>;

/// Number of dmabuf render targets in the rotating pool.
///
/// What:     `const DMABUF_POOL_SIZE: usize = 3;`. Small triple-buffer.
/// Why:      One slot the render loop is drawing into, one the consumer is sampling, one
///           spare in flight — enough that a prompt consumer never stalls the renderer, and
///           small enough that the GPU memory cost stays negligible.
const DMABUF_POOL_SIZE: usize = 3;

/// How many superseded pool generations may stay alive waiting for their releases.
///
/// What:     `const RETIRED_GENERATION_LIMIT: usize = 4;`. At `DMABUF_POOL_SIZE` targets per
///           generation this bounds the retirement list at 12 buffers.
/// Why:      A slot the consumer never releases would otherwise pin its buffer forever, so a
///           storm of resizes could leak the whole GPU. Four generations is far more than the
///           one or two a healthy consumer can have outstanding (it holds at most the frame it
///           is painting), so hitting the limit means the consumer has stopped releasing at
///           all — at which point dropping the oldest generation is the lesser evil, and is
///           logged.
const RETIRED_GENERATION_LIMIT: usize = 4;

/// Bit mask of the slot-index half of an exported `buffer_id`.
const ID_INDEX_MASK: u64 = 0xffff_ffff;

/// Pack a pool generation and a slot index into the exported `buffer_id`.
///
/// What:     `fn encode_buffer_id(generation: u32, index: usize) -> u64`. Generation in the
///           high 32 bits, slot index in the low 32.
/// Why:      A bare slot index is ambiguous across a pool reallocation: a release for slot 1
///           of the old pool would free slot 1 of the NEW pool, which the renderer may then
///           overwrite while the consumer is still sampling it. Stamping the generation makes
///           every release unambiguous.
fn encode_buffer_id(generation: u32, index: usize) -> u64 {
    ((generation as u64) << 32) | (index as u64 & ID_INDEX_MASK)
}

/// Split an exported `buffer_id` back into its generation and slot index.
fn decode_buffer_id(id: u64) -> (u32, usize) {
    ((id >> 32) as u32, (id & ID_INDEX_MASK) as usize)
}

/// What one release message did to the pool.
///
/// What:     `enum Release { Freed(usize), StillHeld(usize), Retired, Unknown }`. `Freed` names
///           a live slot whose LAST hand-out came back, so it is renderable again;
///           `StillHeld` names a live slot that was handed out more than once and has at least
///           one hand-out outstanding; `Retired` means a superseded generation's buffer was
///           dropped; `Unknown` is an id the pool no longer tracks (already fully released, or
///           from a generation the bound forced out).
/// Why:      Lets the caller log — and the tests assert — what happened without reaching into
///           the pool's private state. `StillHeld` is what keeps the exhausted-pool double
///           hand-out safe: the first release must NOT make the slot renderable while the
///           consumer is still sampling the second texture built from the same id.
#[derive(Debug, PartialEq, Eq)]
enum Release {
    /// The id named a live slot, now free for rendering again.
    Freed(usize),
    /// The id named a live slot that is still handed out at least once more.
    StillHeld(usize),
    /// The id named a retired slot; its backing buffer has been dropped.
    Retired,
    /// The id names nothing the pool still tracks; ignored.
    Unknown,
}

/// One pool slot plus its outstanding hand-out count.
struct PoolEntry<S> {
    /// The render target itself.
    slot: S,
    /// How many hand-outs of this slot's id are outstanding; `0` means free.
    ///
    /// A counter rather than a flag because the exhausted-pool fallback in `acquire` can hand
    /// the SAME id out twice; with a flag the first release would free a slot whose second
    /// texture is still on screen.
    in_flight: u32,
}

/// A superseded generation's slot: the buffer plus the releases it is still waiting for.
struct RetiredSlot<S> {
    /// The render target, dropped only once `outstanding` reaches zero.
    ///
    /// Never read after retirement — held only so the consumer's import of this buffer stays
    /// valid until every hand-out of its id has come back.
    #[allow(dead_code)]
    slot: S,
    /// How many hand-outs of this slot's id have not been released yet.
    outstanding: u32,
}

/// A superseded generation, kept alive only for the slots still being sampled.
struct RetiredGeneration<S> {
    /// The generation these slots' ids were stamped with.
    generation: u32,
    /// One entry per slot index: `Some` while awaiting its releases, `None` once dropped.
    slots: Vec<Option<RetiredSlot<S>>>,
}

/// The rotating render-target pool: generation-stamped ids and safe retirement.
///
/// What:     `struct SlotPool<S> { generation, live, retired, next_slot }`. Owns the live
///           slots, hands out `buffer_id`s stamped with the current generation, and — on
///           `replace` — moves the still-in-flight slots of the outgoing pool into `retired`
///           rather than dropping them.
/// Why:      INVARIANT: a slot whose id has been handed to the consumer is neither dropped nor
///           re-bound for rendering until that exact id (generation AND index) comes back
///           through `release`. Reallocating the pool on resize used to break both halves of
///           that — it closed the fds of buffers the host was still displaying, and let a stale
///           release free a NEW slot the renderer would then overwrite mid-sample. Generic over
///           the slot payload so the bookkeeping is unit-testable without a GPU.
struct SlotPool<S> {
    /// Generation stamped into the ids of the current `live` pool.
    generation: u32,
    /// The current pool.
    live: Vec<PoolEntry<S>>,
    /// Superseded generations, kept alive until their in-flight slots are released.
    retired: Vec<RetiredGeneration<S>>,
    /// Round-robin cursor used only when every live slot is in flight.
    next_slot: usize,
}

impl<S> SlotPool<S> {
    /// A fresh pool at generation 0 with every slot free.
    fn new(slots: Vec<S>) -> Self {
        Self {
            generation: 0,
            live: slots
                .into_iter()
                .map(|slot| PoolEntry { slot, in_flight: 0 })
                .collect(),
            retired: Vec::new(),
            next_slot: 0,
        }
    }

    /// `true` when there are no live slots (readback mode, or a failed allocation).
    fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Pick the slot to render into: `(index, exhausted)`.
    ///
    /// What:     Prefers the first free slot; if every slot is in flight it falls back to the
    ///           round-robin cursor and reports `exhausted = true`.
    /// Why:      The same backpressure behaviour as before — the renderer never stalls — with
    ///           the warning left to the caller so this stays pure.
    fn acquire(&mut self) -> Option<(usize, bool)> {
        if self.live.is_empty() {
            return None;
        }
        let (index, exhausted) = match self.live.iter().position(|entry| entry.in_flight == 0) {
            Some(index) => (index, false),
            None => (self.next_slot % self.live.len(), true),
        };
        self.next_slot = (index + 1) % self.live.len();
        Some((index, exhausted))
    }

    /// Borrow a live slot mutably (for binding it as the render target).
    fn slot_mut(&mut self, index: usize) -> Option<&mut S> {
        self.live.get_mut(index).map(|entry| &mut entry.slot)
    }

    /// Count one hand-out of a live slot and return it with the id the consumer echoes back.
    ///
    /// What:     Increments the slot's outstanding hand-out count (saturating, so a pathological
    ///           overflow pins the slot rather than wrapping it to free) and returns the
    ///           generation-stamped id.
    /// Why:      The exhausted-pool fallback can hand the same id out twice; each hand-out then
    ///           needs its own release before the slot is reusable or droppable.
    fn begin_flight(&mut self, index: usize) -> Option<(&S, u64)> {
        let generation = self.generation;
        let entry = self.live.get_mut(index)?;
        entry.in_flight = entry.in_flight.saturating_add(1);
        Some((&entry.slot, encode_buffer_id(generation, index)))
    }

    /// Apply one release message from the consumer.
    ///
    /// What:     Decrements the live slot's hand-out count when the id's generation is the
    ///           current one — freeing it only at zero; decrements the retired slot's count when
    ///           it names a superseded generation, dropping the buffer at zero (and the whole
    ///           retired generation once its last slot is back); ignores anything else.
    /// Why:      This is the half of the invariant that makes a stale release harmless, and the
    ///           counter is what makes a duplicated id safe: a slot is neither re-bound nor
    ///           dropped while any hand-out of it is still outstanding.
    fn release(&mut self, id: u64) -> Release {
        let (generation, index) = decode_buffer_id(id);

        if generation == self.generation {
            return match self.live.get_mut(index) {
                // A release for a slot with nothing outstanding is a duplicate: ignore it
                // rather than "freeing" a slot that was never taken.
                Some(entry) if entry.in_flight == 0 => Release::Unknown,
                Some(entry) => {
                    entry.in_flight -= 1;
                    match entry.in_flight {
                        0 => Release::Freed(index),
                        _ => Release::StillHeld(index),
                    }
                }
                None => Release::Unknown,
            };
        }

        let Some(position) = self
            .retired
            .iter()
            .position(|retired| retired.generation == generation)
        else {
            return Release::Unknown;
        };

        let retired = &mut self.retired[position];
        let Some(entry) = retired.slots.get_mut(index).and_then(|slot| slot.as_mut()) else {
            return Release::Unknown;
        };
        entry.outstanding = entry.outstanding.saturating_sub(1);
        if entry.outstanding > 0 {
            return Release::StillHeld(index);
        }

        // The last hand-out came back: this is the only place a retired buffer is dropped.
        retired.slots[index] = None;
        if retired.slots.iter().all(|slot| slot.is_none()) {
            self.retired.remove(position);
        }
        Release::Retired
    }

    /// Install a freshly allocated pool, retiring the outgoing one's in-flight slots.
    ///
    /// What:     Bumps the generation, drops the outgoing slots that are NOT in flight, moves
    ///           the ones that are into `retired` under their old generation, and resets the
    ///           round-robin cursor. Returns the generation forced out by
    ///           `RETIRED_GENERATION_LIMIT`, if any, for the caller to log.
    /// Why:      The buffers the consumer is still displaying must outlive the pool they came
    ///           from; the ones nobody holds can go immediately.
    fn replace(&mut self, slots: Vec<S>) -> Option<u32> {
        let outgoing = std::mem::replace(
            &mut self.live,
            slots
                .into_iter()
                .map(|slot| PoolEntry { slot, in_flight: 0 })
                .collect(),
        );

        let retained: Vec<Option<RetiredSlot<S>>> = outgoing
            .into_iter()
            .map(|entry| match entry.in_flight {
                0 => None,
                outstanding => Some(RetiredSlot {
                    slot: entry.slot,
                    outstanding,
                }),
            })
            .collect();
        if retained.iter().any(|slot| slot.is_some()) {
            self.retired.push(RetiredGeneration {
                generation: self.generation,
                slots: retained,
            });
        }

        self.generation = self.generation.wrapping_add(1);
        self.next_slot = 0;

        match self.retired.len() > RETIRED_GENERATION_LIMIT {
            true => Some(self.retired.remove(0).generation),
            false => None,
        }
    }
}

/// Which present path the headless backend composites through.
///
/// What:     `pub enum PresentMode { Dmabuf, Readback }`. Selected once at startup from the
///           `KLAMOTTENKISTE_PRESENT` env var (`dmabuf` is the default; `readback` forces
///           the legacy CPU path). Also the value the backend falls back to if the dmabuf
///           pool cannot be allocated or bound on this driver.
/// Why:      A runtime toggle (no rebuild) so a driver that rejects the dmabuf render target
///           can still run via the `glReadPixels` readback the widget already knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PresentMode {
    /// Composite into a dmabuf-backed render target and export it for zero-copy import.
    Dmabuf,
    /// Composite into a plain GLES renderbuffer and hand the widget CPU-readback frames.
    Readback,
}

/// One dmabuf render target in the pool.
///
/// What:     `struct DmabufSlot { buffer: GbmBuffer, dmabuf: Dmabuf }`. `buffer` is the gbm
///           buffer object kept alive so its backing storage lives; `dmabuf` is the exported
///           handle bound as a render target (and described to the consumer).
/// Why:      A slot's in-flight state and its lifetime across pool reallocations are tracked by
///           the owning [`SlotPool`], so the slot itself is only the two GPU handles.
struct DmabufSlot {
    /// The gbm buffer object, kept alive so the exported dmabuf's storage stays valid.
    ///
    /// Never read after construction — held only so the backing bo is not destroyed while
    /// the exported dmabuf (and the consumer's import of it) is still in use.
    #[allow(dead_code)]
    buffer: GbmBuffer,
    /// The exported dmabuf, bound as a render target and described to the consumer.
    dmabuf: Dmabuf,
}

/// A headless GLES backend: a renderer plus its render targets (dmabuf pool + fallback rbo).
///
/// What:     `pub struct HeadlessBackend { renderer, buffer, size, present_mode, allocator,
///           render_fourcc, render_modifiers, pool, current, release_tx,
///           release_rx }`. Owns the renderer (which in turn owns the EGL context, display,
///           and the gbm device behind it), the fallback offscreen renderbuffer, and — in
///           `dmabuf` mode — a `GbmAllocator` plus a small pool of dmabuf render targets.
/// Why:      Mirrors the small slice of `WinitGraphicsBackend`'s surface the render /
///           readback / resize code uses (`renderer`, `bind`, `submit`, `window_size`),
///           plus the dmabuf export/release seam the GTK host imports from.
pub struct HeadlessBackend {
    /// The GLES renderer over the headless EGL context.
    renderer: GlesRenderer,
    /// The fallback offscreen renderbuffer (used in `readback` mode; always allocated).
    buffer: GlesRenderbuffer,
    /// The current framebuffer size in physical pixels.
    size: Size<i32, Physical>,
    /// The active present path (dmabuf export or CPU readback).
    present_mode: PresentMode,
    /// Allocator for the dmabuf pool, kept for reallocation on `resize`.
    allocator: GbmAllocator<File>,
    /// The FourCC every pool target is allocated with (ARGB8888).
    render_fourcc: Fourcc,
    /// The modifier set the pool is allocated with (a render-capable set, or Linear).
    render_modifiers: Vec<Modifier>,
    /// The rotating pool of dmabuf render targets (empty in `readback` mode), plus the
    /// generation bookkeeping that keeps retired slots alive until the consumer releases them.
    pool: SlotPool<DmabufSlot>,
    /// The pool slot bound by the most recent `bind` (cleared by `export_current`).
    current: Option<usize>,
    /// Sending half of the slot-release channel (cloned out to the consumer).
    release_tx: Sender<u64>,
    /// Receiving half: drained before each dmabuf bind to return released slots to the pool.
    release_rx: Receiver<u64>,
}

impl HeadlessBackend {
    /// Borrow the renderer mutably.
    ///
    /// What:     `pub fn renderer(&mut self) -> &mut GlesRenderer`. Mirrors
    ///           `WinitGraphicsBackend::renderer`.
    /// Why:      shm/dmabuf format queries and dmabuf import go through the renderer.
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
    }

    /// The current framebuffer size in physical pixels.
    ///
    /// What:     `pub fn window_size(&self) -> Size<i32, Physical>`. Mirrors
    ///           `WinitGraphicsBackend::window_size`.
    /// Why:      Render and readback size their regions from this.
    pub fn window_size(&self) -> Size<i32, Physical> {
        self.size
    }

    /// The active present path.
    ///
    /// What:     `pub fn present_mode(&self) -> PresentMode`.
    /// Why:      The redraw timer publishes either `latest_dmabuf` (Dmabuf) or `latest_frame`
    ///           via CPU readback (Readback); it branches on this.
    pub fn present_mode(&self) -> PresentMode {
        self.present_mode
    }

    /// A clone of the slot-release sender for the consumer.
    ///
    /// What:     `pub fn release_sender(&self) -> Sender<u64>`.
    /// Why:      `spawn_headless` hands this to `HeadlessHandle` so the GTK host can signal
    ///           when it has finished sampling a dmabuf slot.
    pub fn release_sender(&self) -> Sender<u64> {
        self.release_tx.clone()
    }

    /// Bind the active render target, returning the renderer and its framebuffer.
    ///
    /// What:     `pub fn bind(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>),
    ///           GlesError>`. In `dmabuf` mode it drains the release channel, picks a free
    ///           pool slot (round-robin fallback under backpressure), records it as
    ///           `current`, and binds that slot's dmabuf. In `readback` mode it binds the
    ///           fallback renderbuffer, exactly as before. The returned `GlesTarget` borrows
    ///           the target (not the renderer), so the renderer is handed back alongside it.
    /// Why:      Mirrors `WinitGraphicsBackend::bind`, letting `render.rs`/`screenshot.rs`
    ///           call `state.backend.bind()` unchanged across both present modes.
    pub fn bind(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>), GlesError> {
        if self.present_mode == PresentMode::Dmabuf && !self.pool.is_empty() {
            // Return the slots the consumer finished sampling: live ones become renderable
            // again, retired ones have their buffer dropped here and nowhere else.
            self.drain_releases();

            // Prefer a free slot; under backpressure (all in flight) fall back to a
            // round-robin slot so the renderer never stalls. The strict "not reused until
            // released" contract holds in the normal case where a slot is free. Reusing an
            // in-flight slot hands its id out a second time; the pool counts hand-outs, so the
            // slot only becomes reusable (or droppable) once BOTH releases arrive. It should be
            // rare enough to warn about: a healthy consumer holds at most one frame.
            if let Some((idx, exhausted)) = self.pool.acquire() {
                if exhausted {
                    warn!("dmabuf pool exhausted (all slots in flight); reusing slot {idx}");
                }
                self.current = Some(idx);

                if let Some(slot) = self.pool.slot_mut(idx) {
                    let target = self.renderer.bind(&mut slot.dmabuf)?;
                    return Ok((&mut self.renderer, target));
                }
            }
        }

        let target = self.renderer.bind(&mut self.buffer)?;
        Ok((&mut self.renderer, target))
    }

    /// Bind the offscreen renderbuffer for a CPU readback, claiming no presentation slot.
    ///
    /// What:     `pub fn bind_readback(&mut self) -> Result<(&mut GlesRenderer,
    ///           GlesTarget<'_>), GlesError>`. Binds `self.buffer` — the fallback offscreen
    ///           renderbuffer, which is allocated and resized in BOTH present modes — and
    ///           leaves `current`, `next_slot` and the pool's `in_flight` flags untouched.
    /// Why:      A readback composites a frame nobody presents, so it must not take a
    ///           presentation slot. Going through `bind` in `dmabuf` mode does two harmful
    ///           things: it records the slot it picked as `current`, leaving the backend
    ///           pointing at a target that was drawn into but neither exported nor marked
    ///           `in_flight` (breaking `current`'s documented invariant, "the pool slot bound
    ///           by the most recent `bind`, cleared by `export_current`"); and under
    ///           backpressure — every slot in flight — its round-robin fallback would
    ///           composite the readback over a slot the host is still sampling, corrupting the
    ///           frame on screen. Neither can happen on a target outside the pool.
    pub fn bind_readback(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>), GlesError> {
        let target = self.renderer.bind(&mut self.buffer)?;
        return Ok((&mut self.renderer, target));
    }

    /// Finish the GPU work for the just-rendered dmabuf slot and export a description of it.
    ///
    /// What:     `pub fn export_current(&mut self) -> Option<crate::app::DmabufFrame>`. In
    ///           `dmabuf` mode, for the slot the last `bind` selected: run `glFinish` so all
    ///           compositing writes are complete before the fd is sampled elsewhere, mark the
    ///           slot `in_flight`, and return its plain description (size, FourCC, modifier,
    ///           per-plane fd/offset/stride, slot id). Returns `None` in `readback` mode or
    ///           if no slot was bound.
    /// Why:      SYNC CHOICE — this first correct version uses `glFinish` (a full CPU/GPU
    ///           barrier) rather than a GLES fence the consumer waits on. It is the bluntest
    ///           possible sync, but it is exactly what makes the exported dmabuf safe to
    ///           sample the instant `latest_dmabuf` returns it, and it still removes the
    ///           `glReadPixels` roundtrip that the readback path pays every frame (a device
    ///           readback + row flip + full-frame memcpy). A pipelined fence is a later
    ///           optimisation; correctness first.
    pub fn export_current(&mut self) -> Option<crate::app::DmabufFrame> {
        if self.present_mode != PresentMode::Dmabuf {
            return None;
        }
        let idx = self.current.take()?;

        // Block until the GPU has finished compositing into this slot. Only then are the
        // dmabuf's pixels safe for another importer (the GTK GL context) to sample.
        let _ = self.renderer.with_context(|gl| unsafe { gl.Finish() });

        let (slot, buffer_id) = self.pool.begin_flight(idx)?;
        let dmabuf = &slot.dmabuf;

        let planes: Vec<crate::app::DmabufPlane> = dmabuf
            .handles()
            .zip(dmabuf.offsets())
            .zip(dmabuf.strides())
            .map(|((fd, offset), stride)| crate::app::DmabufPlane {
                fd: fd.as_raw_fd(),
                offset,
                stride,
            })
            .collect();

        let format = dmabuf.format();
        Some(crate::app::DmabufFrame {
            width: self.size.w as u32,
            height: self.size.h as u32,
            fourcc: format.code as u32,
            modifier: u64::from(format.modifier),
            planes,
            buffer_id,
        })
    }

    /// Apply every pending release message from the consumer.
    ///
    /// What:     `fn drain_releases(&mut self)`. Empties the release channel through
    ///           [`SlotPool::release`], logging ids the pool no longer tracks at trace level.
    /// Why:      One place where releases are applied, so `bind` and `resize` cannot disagree
    ///           about the bookkeeping. Ids are never discarded unexamined: a stale one may be
    ///           the last reference holding a retired buffer alive.
    fn drain_releases(&mut self) {
        while let Ok(id) = self.release_rx.try_recv() {
            if self.pool.release(id) == Release::Unknown {
                let (generation, index) = decode_buffer_id(id);
                trace!("dmabuf release for untracked slot {index} of generation {generation}");
            }
        }
    }

    /// Present the composited frame.
    ///
    /// What:     `pub fn submit(&mut self, _damage: Option<&[Rectangle<i32, Physical>]>)
    ///           -> Result<(), GlesError>`. A no-op for the headless backend (there is no
    ///           on-screen surface to swap); the composited pixels live in the offscreen
    ///           target until a readback copies them out or the consumer imports the dmabuf.
    /// Why:      Mirrors `WinitGraphicsBackend::submit` so the redraw call site is
    ///           unchanged.
    pub fn submit(
        &mut self,
        _damage: Option<&[Rectangle<i32, Physical>]>,
    ) -> Result<(), GlesError> {
        Ok(())
    }

    /// Reallocate the render targets at a new size.
    ///
    /// What:     `pub fn resize(&mut self, width: i32, height: i32) -> Result<()>`. A resize to
    ///           the current size returns immediately. Otherwise it
    ///           reallocates the fallback renderbuffer and, in `dmabuf` mode, the whole
    ///           dmabuf pool at the new size. Pending releases are applied FIRST (so a slot the
    ///           consumer has already handed back is not needlessly retired), then the outgoing
    ///           pool is retired rather than dropped: any slot still in flight stays alive,
    ///           under its old generation, until its release arrives. The fresh slots start
    ///           free at the new generation.
    /// Why:      The `resize` control command changes the nested screen size; the winit
    ///           path did this by asking winit for a new inner size. Dropping the outgoing pool
    ///           outright closed dmabuf fds the host was still displaying (a GUI-freezing
    ///           use-after-free in GTK's importer), and let a stale release free a slot of the
    ///           NEW pool.
    pub fn resize(&mut self, width: i32, height: i32) -> Result<()> {
        // A resize to the size we already have would retire a whole generation and reallocate
        // the pool for nothing; the targets are already correct.
        if self.size.w == width && self.size.h == height {
            return Ok(());
        }

        // Allocate everything before committing anything, so a failed allocation leaves the
        // backend consistent at the OLD size rather than half-resized.
        let region: Size<i32, BufferCoord> = (width, height).into();
        let buffer = self
            .renderer
            .create_buffer(Fourcc::Argb8888, region)
            .map_err(|err| anyhow!("allocating the offscreen renderbuffer failed: {err:?}"))?;

        let pool = match self.present_mode {
            PresentMode::Dmabuf => Some(
                allocate_dmabuf_pool(
                    &mut self.allocator,
                    width as u32,
                    height as u32,
                    self.render_fourcc,
                    &self.render_modifiers,
                )
                .context("reallocating the dmabuf pool on resize")?,
            ),
            PresentMode::Readback => None,
        };

        self.buffer = buffer;
        self.size = (width, height).into();

        if let Some(pool) = pool {
            self.drain_releases();
            if let Some(dropped) = self.pool.replace(pool) {
                warn!(
                    "dmabuf pool generation {dropped} never released by the consumer; dropping it"
                );
            }
            self.current = None;
        }
        Ok(())
    }
}

/// Allocate a fresh pool of dmabuf render targets at the given size.
///
/// What:     `fn allocate_dmabuf_pool(allocator, width, height, fourcc, modifiers) ->
///           Result<Vec<DmabufSlot>>`. Creates `DMABUF_POOL_SIZE` gbm buffer objects with
///           the given format/modifiers and exports each as a `Dmabuf`; keeps both alive per
///           slot, all starting free.
/// Why:      Shared by initial setup and `resize`.
fn allocate_dmabuf_pool(
    allocator: &mut GbmAllocator<File>,
    width: u32,
    height: u32,
    fourcc: Fourcc,
    modifiers: &[Modifier],
) -> Result<Vec<DmabufSlot>> {
    let mut pool = Vec::with_capacity(DMABUF_POOL_SIZE);
    for i in 0..DMABUF_POOL_SIZE {
        let buffer = allocator
            .create_buffer(width, height, fourcc, modifiers)
            .with_context(|| format!("allocating dmabuf pool slot {i}"))?;
        let dmabuf = buffer
            .export()
            .with_context(|| format!("exporting dmabuf pool slot {i}"))?;
        pool.push(DmabufSlot { buffer, dmabuf });
    }
    Ok(pool)
}

/// Read the requested present mode from the environment.
///
/// What:     `fn present_mode_from_env() -> PresentMode`. `KLAMOTTENKISTE_PRESENT=readback`
///           forces the CPU path; anything else (including unset) is `Dmabuf`.
/// Why:      Runtime selection with no rebuild, per the fallback requirement.
fn present_mode_from_env() -> PresentMode {
    match std::env::var("KLAMOTTENKISTE_PRESENT") {
        Ok(value) if value.eq_ignore_ascii_case("readback") => PresentMode::Readback,
        _ => PresentMode::Dmabuf,
    }
}

/// Pick the ARGB8888 modifier set the GLES renderer can render into as a dmabuf target.
///
/// What:     `fn render_modifiers_for_argb8888(renderer) -> Vec<Modifier>`. Collects every
///           modifier the EGL display advertises as a *render* format for ARGB8888; if the
///           driver advertises none, falls back to `[Linear]` (broadly importable).
/// Why:      Allocating with a render-capable modifier is what lets `GlesRenderer::bind`
///           build an EGLImage + FBO over the dmabuf and composite into it.
fn render_modifiers_for_argb8888(renderer: &GlesRenderer) -> Vec<Modifier> {
    let mut modifiers: Vec<Modifier> = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .filter(|format| format.code == Fourcc::Argb8888)
        .map(|format| format.modifier)
        .collect();
    if modifiers.is_empty() {
        modifiers.push(Modifier::Linear);
    }
    modifiers
}

/// Open a usable DRM render node as a read/write `File`.
///
/// What:     `fn open_render_node() -> Result<File>`. Tries, in order: the
///           `KABELSALAT_DRM_RENDER_NODE` env override, every `renderD*` node under
///           `/dev/dri`, then `/dev/dri/renderD128` as a last resort. Returns the first
///           node that opens.
/// Why:      The headless EGL display is built from a gbm device wrapping this fd; the
///           exact render node varies by machine, so probe rather than hard-code.
fn open_render_node() -> Result<File> {
    // What:     Collect candidate node paths, most-specific first.
    // Why:      An explicit override wins; otherwise enumerate what the machine has.
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(path) = std::env::var("KABELSALAT_DRM_RENDER_NODE") {
        candidates.push(PathBuf::from(path));
    }

    if let Ok(entries) = std::fs::read_dir("/dev/dri") {
        let mut nodes: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        candidates.extend(nodes);
    }

    candidates.push(PathBuf::from("/dev/dri/renderD128"));

    // What:     Try each candidate; return the first that opens read/write.
    // Why:      gbm/EGL need a writable render node.
    for path in &candidates {
        match File::options().read(true).write(true).open(path) {
            Ok(file) => {
                info!("headless EGL: using DRM render node {}", path.display());
                return Ok(file);
            }
            Err(err) => {
                warn!("headless EGL: cannot open {}: {err}", path.display());
            }
        }
    }

    Err(anyhow!(
        "no usable DRM render node found (set KABELSALAT_DRM_RENDER_NODE to one)"
    ))
}

/// Build the headless GLES backend, the nested output, and the dmabuf state.
///
/// What:     `pub fn init_headless_backend(display_handle: &DisplayHandle, width: u32,
///           height: u32) -> Result<BackendPieces>`. Opens a render node, builds a
///           headless EGL context from a gbm device, creates the `GlesRenderer` and its
///           offscreen renderbuffer, then delegates the Output/dmabuf setup to
///           `finish_backend_setup`.
/// Why:      The default (no-winit) backend seam the compositor is constructed from.
pub fn init_headless_backend(
    display_handle: &DisplayHandle,
    width: u32,
    height: u32,
) -> Result<BackendPieces> {
    // What:     Open the render node. Duplicate the fd (`try_clone`) so the SAME physical
    //           render node backs two gbm devices: one consumed by the EGL display, one kept
    //           for the dmabuf pool allocator (`EGLDisplay::new` takes the device by value).
    // Why:      `GbmDevice` implements `EGLNativeDisplay`, the input to `EGLDisplay::new`;
    //           the allocator needs its own device to create the render targets we bind.
    let node =
        open_render_node().context("opening a DRM render node for the headless EGL backend")?;
    let alloc_node = node
        .try_clone()
        .context("duplicating the render node fd for the dmabuf allocator")?;
    let gbm = GbmDevice::new(node).context("creating a gbm device from the render node")?;
    let alloc_gbm = GbmDevice::new(alloc_node)
        .context("creating the allocator gbm device from the render node")?;

    // What:     Build the headless EGL display/context and the GLES renderer.
    //           `EGLDisplay::new` and `GlesRenderer::new` are `unsafe` (raw EGL/GL
    //           handles); safety here is the same contract smithay's own headless
    //           example relies on — the display outlives the context, which outlives the
    //           renderer, all owned by the returned backend.
    // Why:      This is the GPU render path with no window.
    let egl_display =
        unsafe { EGLDisplay::new(gbm) }.context("creating the headless EGLDisplay")?;
    let egl_context = EGLContext::new(&egl_display).context("creating the headless EGLContext")?;
    let mut renderer =
        unsafe { GlesRenderer::new(egl_context) }.context("creating the headless GlesRenderer")?;

    // What:     Allocate the fallback offscreen renderbuffer at the requested size. Always
    //           allocated so the `readback` present mode (and driver-reject fallback) works
    //           without a rebuild.
    // Why:      `render_output` composites into this in readback mode; in dmabuf mode it is
    //           the safety net if the pool cannot be bound.
    let region: Size<i32, BufferCoord> = (width as i32, height as i32).into();
    let buffer = renderer
        .create_buffer(Fourcc::Argb8888, region)
        .map_err(|err| anyhow!("allocating the offscreen renderbuffer failed: {err:?}"))?;

    // What:     Build the dmabuf pool allocator (RENDERING usage — these are render targets)
    //           and pick a render-capable ARGB8888 modifier set for it.
    // Why:      The pool targets must be allocated with a modifier the renderer can bind an
    //           FBO over, or `bind` fails; RENDERING is the usage that guarantees that.
    let mut allocator = GbmAllocator::new(alloc_gbm, GbmBufferFlags::RENDERING);
    let render_fourcc = Fourcc::Argb8888;
    let render_modifiers = render_modifiers_for_argb8888(&renderer);

    // What:     Choose the present mode from the environment, then — in dmabuf mode — try to
    //           allocate the pool AND test-bind slot 0. Any failure logs a warning and falls
    //           back to `readback` so the compositor still starts on a driver that rejects
    //           the dmabuf render target.
    // Why:      "If a driver rejects the dmabuf path we can fall back without a rebuild."
    let mut present_mode = present_mode_from_env();
    let mut pool: Vec<DmabufSlot> = Vec::new();
    if present_mode == PresentMode::Dmabuf {
        match allocate_dmabuf_pool(
            &mut allocator,
            width,
            height,
            render_fourcc,
            &render_modifiers,
        ) {
            Ok(mut allocated) => {
                // Test-bind slot 0 in a scoped statement so the returned target (which
                // borrows `allocated`) is dropped at the `;` — before we move the pool.
                let bind_result = renderer.bind(&mut allocated[0].dmabuf).map(|_| ());
                match bind_result {
                    Ok(()) => {
                        let modifier = allocated[0].dmabuf.format().modifier;
                        info!(
                            "dmabuf present: rotating pool of {} ARGB8888 targets ({}x{}), modifier {:?}",
                            allocated.len(),
                            width,
                            height,
                            modifier,
                        );
                        pool = allocated;
                    }
                    Err(err) => {
                        warn!(
                            "dmabuf present: renderer rejected the dmabuf target ({err:?}); falling back to readback"
                        );
                        present_mode = PresentMode::Readback;
                    }
                }
            }
            Err(err) => {
                warn!("dmabuf present: pool allocation failed ({err:#}); falling back to readback");
                present_mode = PresentMode::Readback;
            }
        }
    } else {
        info!("readback present: CPU glReadPixels path selected via KLAMOTTENKISTE_PRESENT");
    }

    // What:     Shared Output + dmabuf-global setup (identical to the winit path).
    // Why:      Everything downstream of the renderer is presentation-independent.
    let (output, dmabuf_state, dmabuf_global, dmabuf_feedback) =
        finish_backend_setup(display_handle, width as i32, height as i32, &mut renderer)?;

    let (release_tx, release_rx) = crossbeam_channel::unbounded::<u64>();

    let backend = HeadlessBackend {
        renderer,
        buffer,
        size: (width as i32, height as i32).into(),
        present_mode,
        allocator,
        render_fourcc,
        render_modifiers,
        pool: SlotPool::new(pool),
        current: None,
        release_tx,
        release_rx,
    };

    Ok(BackendPieces {
        backend,
        output,
        dmabuf_state,
        dmabuf_global,
        dmabuf_feedback,
    })
}

/// Build the nested output and the dmabuf protocol state from a ready renderer.
///
/// What:     `fn finish_backend_setup(display_handle: &DisplayHandle, width: i32,
///           height: i32, renderer: &mut GlesRenderer) -> Result<(Output, DmabufState,
///           DmabufGlobal, Option<DmabufFeedback>)>`. Creates the `wl_output` global,
///           sets its mode/transform, queries the render node for dmabuf v4 feedback
///           (falling back to v3), and binds the legacy `wl_drm` EGL path.
/// Why:      The Output/dmabuf construction is identical for the headless and winit
///           paths, so it lives in one place both call.
fn finish_backend_setup(
    display_handle: &DisplayHandle,
    width: i32,
    height: i32,
    renderer: &mut GlesRenderer,
) -> Result<(Output, DmabufState, DmabufGlobal, Option<DmabufFeedback>)> {
    // What:     The output's video mode at the requested resolution.
    // Why:      Describe the nested screen's resolution and refresh to clients.
    let mode = Mode {
        size: (width, height).into(),
        refresh: OUTPUT_REFRESH_MHZ,
    };

    // What:     Create the output object (0x0 mm physical size for a virtual screen).
    // Why:      The one screen the fixture presents.
    let output = Output::new(
        "nested".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Monochromatic".into(),
            model: "NestedWaylandSession".into(),
        },
    );

    // What:     Register the `wl_output` global (kept alive by the `Output` itself).
    // Why:      Advertise the screen to clients.
    let _global = output.create_global::<crate::state::Compositor>(display_handle);

    // What:     Make the mode current with the identity transform and origin position.
    // Why:      Render the composited frame top-down directly into the target's memory, so the
    //           raw FBO handed to GTK as a dmabuf is already upright (row 0 = top). GL's
    //           framebuffer origin is bottom-left, but `render_output` bakes the output
    //           transform into its projection: with `Transform::Normal` the scene is written so
    //           that buffer memory row 0 is the top of the image — exactly what GTK/DRM sample
    //           top-down. The old `Flipped180` produced bottom-up memory, which forced the
    //           readback to flip on the CPU and left the zero-copy dmabuf upside-down. With the
    //           identity transform the readback needs no flip either (see `render::read_frame_rgba`).
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    // What:     Walk renderer -> EGL context -> EGL display -> the DRM render node.
    // Why:      dmabuf v4 feedback needs the render node's device id.
    let render_node = EGLDevice::device_for_display(renderer.egl_context().display())
        .and_then(|device| device.try_get_render_node());

    let mut dmabuf_state = DmabufState::new();

    // What:     Prefer dmabuf v4 modifier feedback when the render node is known; else v3.
    // Why:      Never fail if the render node cannot be determined.
    let (dmabuf_global, dmabuf_feedback) = match render_node {
        Ok(Some(node)) => {
            let formats = renderer.dmabuf_formats();
            let feedback = DmabufFeedbackBuilder::new(node.dev_id(), formats)
                .build()
                .context("building dmabuf v4 default feedback failed")?;
            let global = dmabuf_state
                .create_global_with_default_feedback::<crate::state::Compositor>(
                    display_handle,
                    &feedback,
                );
            info!("dmabuf: advertising v4 with modifier feedback");
            (global, Some(feedback))
        }
        _ => {
            warn!("dmabuf: no render node available, falling back to v3");
            let formats = renderer.dmabuf_formats();
            let global =
                dmabuf_state.create_global::<crate::state::Compositor>(display_handle, formats);
            (global, None)
        }
    };

    // What:     Bind the legacy EGL `wl_drm` path (from `ImportEgl`).
    // Why:      Mesa accepts either dmabuf v4 OR this wl_drm path for acceleration.
    if renderer.bind_wl_display(display_handle).is_ok() {
        info!("EGL hardware-acceleration (wl_drm) enabled");
    }

    Ok((output, dmabuf_state, dmabuf_global, dmabuf_feedback))
}

/// Legacy winit nested-window backend (preserved, off by default).
///
/// What:     Everything below is compiled only with `#[cfg(feature = "backend_winit")]`.
///           It builds a nested winit window + its GLES renderer via
///           `smithay::backend::winit::init_from_attributes`, then reuses
///           `finish_backend_setup` for the Output/dmabuf state.
/// Why:      Keep the original winit entry point as cfg-gated source rather than deleting
///           it, so the upstream diff stays small.
#[cfg(feature = "backend_winit")]
mod winit_backend {
    use super::{finish_backend_setup, BackendPieces};
    use anyhow::Result;
    use smithay::{
        backend::{
            renderer::gles::GlesRenderer,
            winit::{self, WinitEventLoop},
        },
        reexports::{
            wayland_server::DisplayHandle,
            winit::{dpi::PhysicalSize, window::WindowAttributes},
        },
    };

    /// Build the winit backend, the nested output, and the dmabuf state.
    ///
    /// What:     `pub fn init_backend(display_handle: &DisplayHandle, width: u32, height:
    ///           u32) -> Result<(BackendPieces, WinitEventLoop)>`.
    /// Why:      The original nested-window entry point.
    pub fn init_backend(
        display_handle: &DisplayHandle,
        width: u32,
        height: u32,
    ) -> Result<(BackendPieces, WinitEventLoop)> {
        let attributes = WindowAttributes::default()
            .with_title("nested-wayland-session")
            .with_inner_size(PhysicalSize::new(width, height));

        let (mut backend, winit) = winit::init_from_attributes::<GlesRenderer>(attributes)
            .map_err(|err| anyhow::anyhow!("winit backend init failed: {err}"))?;

        let (output, dmabuf_state, dmabuf_global, dmabuf_feedback) = finish_backend_setup(
            display_handle,
            width as i32,
            height as i32,
            backend.renderer(),
        )?;

        Ok((
            BackendPieces {
                backend,
                output,
                dmabuf_state,
                dmabuf_global,
                dmabuf_feedback,
            },
            winit,
        ))
    }
}

#[cfg(feature = "backend_winit")]
pub use winit_backend::init_backend;

/// Unit tests for the pool's generation/release bookkeeping.
///
/// What:     Exercises [`SlotPool`] with a plain payload that counts its own drops, so the
///           invariant ("a slot handed out is neither dropped nor re-bound until its exact id
///           comes back") is testable without a GPU, an EGL context, or a gbm device.
/// Why:      This bookkeeping is what a resize storm stresses, and getting it wrong closes fds
///           the host is still displaying — a crash the compositor cannot observe itself.
#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    /// A payload that bumps a shared counter when it is dropped.
    struct Tracked(Rc<Cell<usize>>);

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// `count` tracked slots sharing one drop counter.
    fn tracked_pool(drops: &Rc<Cell<usize>>, count: usize) -> Vec<Tracked> {
        (0..count).map(|_| Tracked(Rc::clone(drops))).collect()
    }

    /// Acquire a slot and hand it out, returning its exported id.
    fn hand_out<S>(pool: &mut SlotPool<S>) -> u64 {
        let (index, _) = pool.acquire().expect("a non-empty pool always acquires");
        pool.begin_flight(index)
            .expect("the acquired slot exists")
            .1
    }

    #[test]
    fn buffer_ids_round_trip_generation_and_index() {
        assert_eq!(decode_buffer_id(encode_buffer_id(0, 0)), (0, 0));
        assert_eq!(decode_buffer_id(encode_buffer_id(7, 2)), (7, 2));
        assert_eq!(
            decode_buffer_id(encode_buffer_id(u32::MAX, 2)),
            (u32::MAX, 2)
        );
        // Distinct generations never collide on the same slot index.
        assert_ne!(encode_buffer_id(1, 0), encode_buffer_id(2, 0));
    }

    #[test]
    fn a_released_live_slot_becomes_renderable_again() {
        let mut pool = SlotPool::new(vec![0u32, 1, 2]);
        let id = hand_out(&mut pool);
        // Slot 0 is in flight, so the next acquire moves on.
        assert_eq!(pool.acquire(), Some((1, false)));
        assert_eq!(pool.release(id), Release::Freed(0));
        assert_eq!(pool.acquire(), Some((0, false)));
    }

    #[test]
    fn every_slot_in_flight_falls_back_to_round_robin() {
        let mut pool = SlotPool::new(vec![0u32, 1, 2]);
        for _ in 0..3 {
            hand_out(&mut pool);
        }
        assert_eq!(pool.acquire(), Some((0, true)));
        assert_eq!(pool.acquire(), Some((1, true)));
    }

    #[test]
    fn a_slot_handed_out_twice_needs_two_releases() {
        let drops = Rc::new(Cell::new(0));
        let mut pool = SlotPool::new(tracked_pool(&drops, 1));

        // Fill the pool, then let the exhausted fallback re-hand slot 0: the SAME id twice.
        let first = hand_out(&mut pool);
        let (index, exhausted) = pool.acquire().expect("a non-empty pool always acquires");
        assert!(exhausted, "every slot is in flight");
        let second = pool
            .begin_flight(index)
            .expect("the acquired slot exists")
            .1;
        assert_eq!(first, second, "the fallback re-hands an in-flight id");

        // The first release must NOT free the slot the consumer is still sampling.
        assert_eq!(pool.release(second), Release::StillHeld(0));
        assert_eq!(
            pool.acquire(),
            Some((0, true)),
            "slot 0 is still in flight, so acquire is still exhausted"
        );
        // That fallback acquire took no hand-out, so two releases still settle the two above.
        assert_eq!(pool.release(second), Release::Freed(0));
        assert_eq!(pool.acquire(), Some((0, false)), "now it is genuinely free");
        // A third release for the same id is a duplicate and changes nothing.
        assert_eq!(pool.release(second), Release::Unknown);
        assert_eq!(drops.get(), 0, "no live slot was dropped along the way");
    }

    #[test]
    fn a_twice_handed_out_slot_stays_retired_until_both_releases() {
        let drops = Rc::new(Cell::new(0));
        let mut pool = SlotPool::new(tracked_pool(&drops, 1));
        let id = hand_out(&mut pool);
        // The single slot is exhausted, so this hands the same id out again.
        let (index, exhausted) = pool.acquire().expect("a non-empty pool always acquires");
        assert!(exhausted);
        pool.begin_flight(index);

        assert_eq!(pool.replace(tracked_pool(&drops, 1)), None);
        assert_eq!(
            drops.get(),
            0,
            "the twice-held slot is retired, not dropped"
        );

        assert_eq!(pool.release(id), Release::StillHeld(0));
        assert_eq!(drops.get(), 0, "one release is not enough to drop it");
        assert_eq!(pool.release(id), Release::Retired);
        assert_eq!(drops.get(), 1, "the second release drops the retired slot");
        assert_eq!(pool.release(id), Release::Unknown);
    }

    #[test]
    fn resize_keeps_in_flight_slots_alive_and_drops_the_free_ones() {
        let drops = Rc::new(Cell::new(0));
        let mut pool = SlotPool::new(tracked_pool(&drops, 3));
        let id = hand_out(&mut pool);

        assert_eq!(pool.replace(tracked_pool(&drops, 3)), None);
        // The two slots nobody held went immediately; the in-flight one is retired, not freed.
        assert_eq!(drops.get(), 2, "free outgoing slots are dropped at once");

        assert_eq!(pool.release(id), Release::Retired);
        assert_eq!(drops.get(), 3, "the retired slot dies with its release");
        // Its generation is gone with it, so a repeat release is a no-op.
        assert_eq!(pool.release(id), Release::Unknown);
        assert_eq!(drops.get(), 3);
    }

    #[test]
    fn a_stale_release_never_frees_a_slot_of_the_new_pool() {
        let mut pool = SlotPool::new(vec![0u32, 1, 2]);
        let stale = hand_out(&mut pool);
        pool.replace(vec![10u32, 11, 12]);

        // Slot 0 of the NEW pool is handed out and must stay in flight.
        let fresh = hand_out(&mut pool);
        assert_ne!(stale, fresh, "the same index carries a new generation");

        assert_eq!(pool.release(stale), Release::Retired);
        assert_eq!(
            pool.acquire(),
            Some((1, false)),
            "the stale release must not have freed new slot 0"
        );
        assert_eq!(pool.release(fresh), Release::Freed(0));
    }

    #[test]
    fn ids_from_a_generation_with_nothing_outstanding_are_ignored() {
        let mut pool = SlotPool::new(vec![0u32, 1, 2]);
        let id = hand_out(&mut pool);
        assert_eq!(pool.release(id), Release::Freed(0));
        // Nothing was in flight, so nothing is retained across the replace.
        pool.replace(vec![10u32, 11, 12]);
        assert_eq!(pool.release(id), Release::Unknown);
        // An index past the end of the live pool is ignored rather than panicking.
        assert_eq!(pool.release(encode_buffer_id(1, 99)), Release::Unknown);
    }

    #[test]
    fn retired_generations_are_bounded() {
        let drops = Rc::new(Cell::new(0));
        let mut pool = SlotPool::new(tracked_pool(&drops, 1));
        let first = hand_out(&mut pool);

        // Each round retires one never-released slot.
        for _ in 0..RETIRED_GENERATION_LIMIT {
            assert_eq!(pool.replace(tracked_pool(&drops, 1)), None);
            hand_out(&mut pool);
        }
        assert_eq!(drops.get(), 0, "nothing released, so nothing dropped yet");

        // One retirement too many: the oldest generation is forced out and reported.
        let (generation, _) = decode_buffer_id(first);
        assert_eq!(pool.replace(tracked_pool(&drops, 1)), Some(generation));
        assert_eq!(drops.get(), 1, "the forced-out generation is dropped");
        assert_eq!(pool.release(first), Release::Unknown);
    }
}
