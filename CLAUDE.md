# videoEditor

## Layout

`src/main.rs` is module declarations and `fn main` only. The UI is one big
`State` (`src/state.rs`) whose behaviour is split across modules by concern,
each adding its own `impl State`: `layout` (geometry, the timeline's visible
window, and hit testing), `input` (pointer gestures), `edit` (timeline
mutations and undo), `session` (project files), `compound` (compound clips:
combining, opening and closing them), `freeze` (freeze frames: the frame
grab, its PNG beside the project, and the gap it fills), `picker` (the colour picker: its
state, the eyedropper and its gradients; `color.rs` holds the colour maths
and the recent colours), `styles` (saved title styles: a title's look under a
name, applied from the style box on the effects tab),
`canvas` (project format and
its popup), and `render` / `toolbar` / `timeline_view` / `effects_view`
(drawing).

Effects are modelled in `effects.rs`, which also holds the blur arithmetic
both renderers follow: `blur.rs` + `blur.wgsl` run it on the GPU for the
preview, `export/blur.rs` on the CPU for the file. A change to how a blur is
computed lands in all three, and `blur::tests` checks the shader against the
CPU path.

A title is drawn once into a straight-RGBA picture by `title::render` (a box
behind each line, then shadow, outline, fill; faces from `fonts.rs`). The preview uploads that picture and
the export takes it as a still, so both show the same title. New behaviour
joins the module that matches its concern rather than going back into
`main.rs`.

## Typography

Font sizes come from the type scale in `src/theme.rs` (`TYPE_SM`, `TYPE_MD`,
`TYPE_LG`, `TYPE_XL`, `TYPE_ICON`). Never write a font size as a bare literal.

New chrome picks an existing step. Adding a step is a deliberate decision, not
a way to get a value half a point off one that already exists.

`TYPE_SM` (11pt) is the floor. Nothing renders text below it. The timeline
ruler labels and the media pool's format line used to sit a point under it and
read as a squint rather than as small print.

Every constant named `*_SIZE` is a step on that scale, so a stray literal shows
up with:

    grep -rnE "_SIZE: f32 = [0-9]" src/

That should return nothing. Measurements that are not type get a different
suffix (`POOL_CLOSE_BOX`, `SPLITTER_ACTIVE_W`, `TRACK_LANE_MIN_H`).

Sizes are in logical points, not pixels. The projection divides by
`State::scale`, so a constant holds its physical size across displays and a
future UI zoom folds in at that one place rather than at every call site.

`theme.rs` holds every colour and metric. Drawing modules glob-import it, so a
new value goes there rather than at its call site.

What outlives a project — the recent colours, the saved title styles — is
kept in the user's config directory through `config.rs`
(`~/.config/videoEditor/`), not in the project file.

## Editing a project file from outside

A project (`*.vedit`) is RON, written by `project::write`; the user's live
ones are in `~/Videos/RUVE_projects/`. The editor watches the open file
(`reload.rs`) and takes a change in place as one undo step, so edits made to
the file show up in the running app without reopening it.

- A change that only adds sources to the pool (a new entry under
  `sources`, nothing else touched) is taken at once, unsaved work or not.
  Anything else needs the user to save first: with unsaved work in the app,
  it waits and the toolbar's reload button lights up instead.
- Sources are matched by id (titles, compounds) and by path (media), so keep
  existing ids. A new source needs an id unused in the file; a new clip needs
  a clip id unused on its timeline.
- A clip's `transform` is an offset from centred in canvas widths and
  heights, and a scale on the fit (1.0 fills without cropping). A title's
  sizes are fractions of the canvas height; colours are straight sRGB 0 to 1.
- Check the file before the app sees it:
  `target/release/videoEditor --check <file>` prints `ok: ...` or the
  problems, and exits non-zero on any.
