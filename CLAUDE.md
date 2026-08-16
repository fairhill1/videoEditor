# videoEditor

## Typography

Font sizes come from the type scale in `src/main.rs` (`TYPE_SM`, `TYPE_MD`,
`TYPE_LG`, `TYPE_XL`, `TYPE_ICON`). Never write a font size as a bare literal.

New chrome picks an existing step. Adding a step is a deliberate decision, not
a way to get a value half a point off one that already exists.

`TYPE_SM` (11pt) is the floor. Nothing renders text below it. The timeline
ruler labels and the media pool's format line used to sit a point under it and
read as a squint rather than as small print.

Every constant named `*_SIZE` is a step on that scale, so a stray literal shows
up with:

    grep -nE "_SIZE: f32 = [0-9]" src/main.rs

That should return nothing. Measurements that are not type get a different
suffix (`POOL_CLOSE_BOX`, `SPLITTER_ACTIVE_W`, `TRACK_LANE_MIN_H`).

Sizes are in logical points, not pixels. The projection divides by
`State::scale`, so a constant holds its physical size across displays and a
future UI zoom folds in at that one place rather than at every call site.
