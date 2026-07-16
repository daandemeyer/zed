use crate::prelude::*;
use gpui::{Animation, AnimationExt, FontWeight};
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation as _;

#[derive(IntoElement)]
pub struct LoadingLabel {
    base: Label,
    text: SharedString,
}

impl LoadingLabel {
    pub fn new(text: impl Into<SharedString>) -> Self {
        let text = text.into();
        LoadingLabel {
            base: Label::new(text.clone()),
            text,
        }
    }
}

impl LabelCommon for LoadingLabel {
    fn size(mut self, size: LabelSize) -> Self {
        self.base = self.base.size(size);
        self
    }

    fn weight(mut self, weight: FontWeight) -> Self {
        self.base = self.base.weight(weight);
        self
    }

    fn line_height_style(mut self, line_height_style: LineHeightStyle) -> Self {
        self.base = self.base.line_height_style(line_height_style);
        self
    }

    fn color(mut self, color: Color) -> Self {
        self.base = self.base.color(color);
        self
    }

    fn strikethrough(mut self) -> Self {
        self.base = self.base.strikethrough();
        self
    }

    fn italic(mut self) -> Self {
        self.base = self.base.italic();
        self
    }

    fn alpha(mut self, alpha: f32) -> Self {
        self.base = self.base.alpha(alpha);
        self
    }

    fn underline(mut self) -> Self {
        self.base = self.base.underline();
        self
    }

    fn truncate(mut self) -> Self {
        self.base = self.base.truncate();
        self
    }

    fn single_line(mut self) -> Self {
        self.base = self.base.single_line();
        self
    }

    fn buffer_font(mut self, cx: &App) -> Self {
        self.base = self.base.buffer_font(cx);
        self
    }

    fn inline_code(mut self, cx: &App) -> Self {
        self.base = self.base.inline_code(cx);
        self
    }
}

fn grapheme_boundaries(text: &str) -> Vec<usize> {
    text.grapheme_indices(true)
        .map(|(byte_index, _)| byte_index)
        .chain(std::iter::once(text.len()))
        .collect()
}

fn visible_byte_end(delta: f32, grapheme_boundaries: &[usize]) -> usize {
    let grapheme_count = grapheme_boundaries.len().saturating_sub(1);
    let grapheme_index = ((delta * grapheme_count as f32).round() as usize).min(grapheme_count);
    grapheme_boundaries
        .get(grapheme_index)
        .copied()
        .or_else(|| grapheme_boundaries.last().copied())
        .unwrap_or_default()
}

impl RenderOnce for LoadingLabel {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let text = self.text.clone();
        let grapheme_boundaries = grapheme_boundaries(&text);
        let grapheme_count = grapheme_boundaries.len().saturating_sub(1);
        let type_on_fps = grapheme_count.clamp(1, 30) as u32;

        self.base
            .color(Color::Muted)
            .with_animations(
                "loading_label",
                vec![
                    Animation::new(Duration::from_secs(1)),
                    Animation::new(Duration::from_secs(1)).repeat(),
                ],
                move |mut label, animation_ix, delta| {
                    match animation_ix {
                        0 => {
                            let byte_end = visible_byte_end(delta, &grapheme_boundaries);
                            let visible_text = SharedString::new(&text[0..byte_end]);
                            label.set_text(visible_text);
                        }
                        1 => match delta {
                            ..0.25 => label.set_text(text.clone()),
                            ..0.5 => label.set_text(format!("{}.", text)),
                            ..0.75 => label.set_text(format!("{}..", text)),
                            _ => label.set_text(format!("{}...", text)),
                        },
                        _ => {}
                    }
                    label
                },
            )
            .with_frame_intervals([
                Some(Duration::from_secs_f64(1.0 / type_on_fps as f64)),
                Some(Duration::from_millis(250)),
            ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_on_cadence_visits_every_grapheme_boundary() {
        for text in ["abcdefghijklm", "aé🦀z", "e\u{301}", "👨‍👩‍👧‍👦"] {
            let boundaries = grapheme_boundaries(text);
            let grapheme_count = boundaries.len() - 1;
            let frame_interval = Duration::from_secs_f64(1.0 / grapheme_count.max(1) as f64);

            for tick in 0..=grapheme_count {
                let elapsed = frame_interval * tick as u32;
                let delta = (elapsed.as_secs_f32() / Duration::from_secs(1).as_secs_f32()).min(1.0);
                assert_eq!(visible_byte_end(delta, &boundaries), boundaries[tick]);
            }
        }
    }

    #[test]
    fn empty_type_on_text_has_a_valid_boundary() {
        let boundaries = grapheme_boundaries("");
        assert_eq!(boundaries, vec![0]);
        assert_eq!(visible_byte_end(0.0, &boundaries), 0);
        assert_eq!(visible_byte_end(1.0, &boundaries), 0);
    }
}
