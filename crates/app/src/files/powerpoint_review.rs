use super::*;
use ::files::office::FrameReview;

pub(super) fn button(frames: Vec<FrameReview>) -> Button {
    Button::new("powerpoint-review")
        .small()
        .ghost()
        .label(t!("files.pptx_review", count = frames.len()).into_owned())
        .on_click(move |_, window, cx| open(frames.clone(), window, cx))
}

pub(super) fn open(mut frames: Vec<FrameReview>, window: &mut Window, cx: &mut App) {
    frames.sort_by_key(|frame| {
        (
            !frame.overflow,
            frame.slide.unwrap_or(usize::MAX),
            frame.frame,
        )
    });
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let rows = frames.iter().enumerate().map(|(index, frame)| {
            let location = match (frame.slide, frame.notes) {
                (Some(slide), false) if frame.paragraph => {
                    t!("files.pptx_label", slide = slide, label = frame.frame).into_owned()
                }
                (Some(slide), false) => {
                    t!("files.pptx_frame", slide = slide, frame = frame.frame).into_owned()
                }
                (Some(slide), true) => {
                    t!("files.pptx_notes_frame", slide = slide, frame = frame.frame).into_owned()
                }
                (None, _) => format!("{} · {}", frame.part, frame.frame),
            };
            let status = if frame.overflow {
                t!("files.pptx_frame_overflow").into_owned()
            } else {
                t!("files.pptx_frame_shrunk", percent = frame.scale_percent).into_owned()
            };
            let mut source = frame
                .source
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if let Some((end, _)) = source.char_indices().nth(100) {
                source.truncate(end);
                source.push('…');
            }
            let copied = frame.translated.clone();
            v_flex()
                .gap_2()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .flex_wrap()
                        .justify_between()
                        .gap_2()
                        .child(div().font_semibold().child(location))
                        .child(
                            Button::new(("copy-pptx-frame", index))
                                .small()
                                .ghost()
                                .icon(Lucide::Copy)
                                .label(t!("action.copy").into_owned())
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()))
                                }),
                        ),
                )
                .child(
                    div()
                        .text_color(if frame.overflow {
                            theme.warning
                        } else {
                            theme.muted_foreground
                        })
                        .child(status),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(t!("files.pptx_frame_source", source = source).into_owned()),
                )
                .child(div().child(frame.translated.clone()))
        });
        dialog
            .title(t!("files.pptx_review_title").into_owned())
            .w(px(680.))
            .child(
                v_flex()
                    .id("powerpoint-review-scroll")
                    .gap_3()
                    .text_sm()
                    .max_h(px(480.))
                    .overflow_y_scroll()
                    .children(rows),
            )
    });
}
