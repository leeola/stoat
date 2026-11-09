use crate::{cells, ApcScene};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use stoatty_protocol::command::{self, BarCommand, BarsCommand};

/// A solid sub-cell rectangle drawn off the cell grid.
///
/// A gutter packs several variable-width status or git bars and a hairline
/// separator into a fraction of a cell; `Bar` is one such fill. All four
/// coordinates are in **sixteenths of a cell** (16 = one cell) relative to the
/// render area's top-left, so a bar can be a fraction of a cell wide and track
/// live font zoom. There is no cell fallback: a bar is inherently sub-cell.
pub struct Bar {
    /// Left edge in sixteenths, along the cell width, from the area's left.
    /// A negative value overhangs the area to the left.
    pub x: i16,
    /// Top edge in sixteenths, along the cell height, from the area's top.
    /// A negative value overhangs the area above.
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub color: [u8; 3],
}

impl Bar {
    /// Write the bar into `scene`, placed in `area`, as one that hides what lies
    /// under it as a panel's body does.
    ///
    /// The components written before it draw nothing inside its rect, and a
    /// pane's pool composite leaves the rect alone, so the bar holds its place
    /// over a pane that glides or rests composited beneath it. Write it after
    /// the content it covers.
    ///
    /// See also:
    /// - [`BarCommand::occludes`] for how the terminal layers it.
    pub fn render_occluding(self, area: Rect, scene: &mut ApcScene) {
        self.encode(area, true, None, scene);
    }

    /// Write the bar into `scene`, placed in `area`, as a surface striped with
    /// `hatch` over its color.
    ///
    /// A hatched bar draws before every plain bar, so the marks over its rect
    /// show whatever order they were written in.
    ///
    /// See also:
    /// - [`BarCommand::hatch`] for the stripes and the layering.
    pub fn render_hatched(self, hatch: [u8; 3], area: Rect, scene: &mut ApcScene) {
        self.encode(area, false, Some(hatch), scene);
    }

    fn encode(self, area: Rect, occludes: bool, hatch: Option<[u8; 3]>, scene: &mut ApcScene) {
        let x = cells::to_sixteenths(area.x, self.x);
        let y = cells::to_sixteenths(area.y, self.y);

        command::encode_bar_into(
            scene.buffer(),
            &BarCommand {
                x,
                y,
                width: self.width,
                height: self.height,
                color: self.color,
                occludes,
                hatch,
            },
        );
    }
}

impl StatefulWidget for Bar {
    type State = ApcScene;

    fn render(self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        self.encode(area, false, None, scene);
    }
}

/// Bars gathered across a widget's rows, to go out as one `bars` frame per
/// color.
///
/// A gutter draws its marks a bar at a time, and a frame per bar spends most of
/// its bytes on the frame itself. The batch groups the bars pushed into it by
/// color, and [`Self::flush`] writes each group as one frame. The groups go out
/// in the order their first bars arrived, so the draw order between colors
/// changes. Batch only bars that do not overlap one another.
///
/// A scene without the batch protocol, as [`ApcScene::set_batch_protocol`]
/// declares it, takes one `bar` per bar, in the same group order.
#[derive(Default)]
pub struct BarBatch {
    groups: Vec<BarsCommand>,
}

impl BarBatch {
    /// Add `bar`, placed in `area` as the [`Bar`] render places it.
    pub fn push(&mut self, area: Rect, bar: Bar) {
        let entry = (
            cells::to_sixteenths(area.x, bar.x),
            cells::to_sixteenths(area.y, bar.y),
            bar.width,
            bar.height,
        );
        match self
            .groups
            .iter_mut()
            .find(|group| group.color == bar.color)
        {
            Some(group) => group.bars.push(entry),
            None => self.groups.push(BarsCommand {
                color: bar.color,
                bars: vec![entry],
            }),
        }
    }

    /// Write the gathered bars into `scene` and empty the batch.
    pub fn flush(&mut self, scene: &mut ApcScene) {
        let batches = scene.batches;
        let out = scene.buffer();
        for group in self.groups.drain(..) {
            if batches {
                command::encode_bars_into(out, &group);
                continue;
            }

            for (x, y, width, height) in group.bars {
                command::encode_bar_into(
                    out,
                    &BarCommand {
                        x,
                        y,
                        width,
                        height,
                        color: group.color,
                        occludes: false,
                        hatch: None,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bar, BarBatch};
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{decode_stream, encode_bar, BarCommand, BarsCommand, Command};

    /// A bar sits at absolute sixteenths. An occluding render writes the same
    /// bar with its flag set, and a hatched render with its hatch color.
    #[test]
    fn emits_a_bar_at_absolute_sixteenths_plain_occluding_or_hatched() {
        let bar = || Bar {
            x: 3,
            y: 0,
            width: 5,
            height: 16,
            color: [10, 20, 30],
        };
        let area = Rect::new(2, 4, 1, 1);
        let mut plain = ApcScene::new();
        bar().render(
            area,
            &mut Buffer::empty(Rect::new(0, 0, 80, 24)),
            &mut plain,
        );
        let mut occluding = ApcScene::new();
        bar().render_occluding(area, &mut occluding);
        let mut hatched = ApcScene::new();
        bar().render_hatched([40, 50, 60], area, &mut hatched);

        let expected = |occludes, hatch| {
            encode_bar(&BarCommand {
                x: 2 * 16 + 3,
                y: 4 * 16,
                width: 5,
                height: 16,
                color: [10, 20, 30],
                occludes,
                hatch,
            })
        };
        assert_eq!(
            [
                plain.buffer().clone(),
                occluding.buffer().clone(),
                hatched.buffer().clone(),
            ],
            [
                expected(false, None),
                expected(true, None),
                expected(false, Some([40, 50, 60])),
            ],
        );
    }

    #[test]
    fn a_negative_anchor_overhangs_the_area() {
        let mut scene = ApcScene::new();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));

        Bar {
            x: -4,
            y: -16,
            width: 5,
            height: 16,
            color: [10, 20, 30],
        }
        .render(Rect::new(2, 4, 1, 1), &mut buf, &mut scene);

        let expected = encode_bar(&BarCommand {
            x: 2 * 16 - 4,
            y: 4 * 16 - 16,
            width: 5,
            height: 16,
            color: [10, 20, 30],
            occludes: false,
            hatch: None,
        });
        assert_eq!(
            scene.buffer().as_slice(),
            expected.as_slice(),
            "the bar sits left of and above the area origin"
        );
    }

    /// A batch sends one `bars` frame per color, in the order each color first
    /// arrived, and a host without the batch protocol one `bar` per bar in that
    /// order. Each bar sits where its own render puts it.
    #[test]
    fn a_batch_groups_its_bars_by_color() {
        let (red, blue) = ([200, 0, 0], [0, 0, 200]);
        let flushed = |batches: bool| {
            let mut scene = ApcScene::new();
            scene.set_batch_protocol(batches);
            let mut batch = BarBatch::default();
            for (y, color) in [(0, red), (16, blue), (32, red)] {
                let bar = Bar {
                    x: 0,
                    y,
                    width: 2,
                    height: 16,
                    color,
                };
                batch.push(Rect::new(1, 2, 4, 4), bar);
            }
            batch.flush(&mut scene);
            decode_stream(scene.buffer())
        };
        let single = |y, color| {
            Command::Bar(BarCommand {
                x: 16,
                y,
                width: 2,
                height: 16,
                color,
                occludes: false,
                hatch: None,
            })
        };

        assert_eq!(
            flushed(true),
            [
                Command::Bars(BarsCommand {
                    color: red,
                    bars: vec![(16, 32, 2, 16), (16, 64, 2, 16)],
                }),
                Command::Bars(BarsCommand {
                    color: blue,
                    bars: vec![(16, 48, 2, 16)],
                }),
            ],
        );
        assert_eq!(
            flushed(false),
            [single(32, red), single(64, red), single(48, blue)],
        );
    }
}
