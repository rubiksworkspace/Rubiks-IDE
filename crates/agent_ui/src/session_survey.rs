//! The end-of-session survey for Rubiks research: NASA-TLX's six workload
//! scales (0–100 in steps of 10) and a sense-of-agency rating (1–7), saved
//! as a `survey` record with the session's research data.

use std::time::Instant;

use gpui::{DismissEvent, EventEmitter, FocusHandle, Focusable};
use ui::{ButtonStyle, prelude::*};
use workspace::{ModalView, Workspace};

/// The NASA-TLX scales: record key, label, question, and the scale's ends.
const TLX_SCALES: [(&str, &str, &str, &str, &str); 6] = [
    (
        "mentalDemand",
        "Mental demand",
        "How mentally demanding was the task?",
        "Very low",
        "Very high",
    ),
    (
        "physicalDemand",
        "Physical demand",
        "How physically demanding was the task?",
        "Very low",
        "Very high",
    ),
    (
        "temporalDemand",
        "Temporal demand",
        "How hurried or rushed was the pace of the task?",
        "Very low",
        "Very high",
    ),
    (
        "performance",
        "Performance",
        "How successful were you in doing what you set out to do?",
        "Perfect",
        "Failure",
    ),
    (
        "effort",
        "Effort",
        "How hard did you have to work to reach your level of performance?",
        "Very low",
        "Very high",
    ),
    (
        "frustration",
        "Frustration",
        "How insecure, discouraged, irritated, stressed or annoyed were you?",
        "Very low",
        "Very high",
    ),
];

const AGENCY_QUESTION: &str = "How much did you feel in control of what the agent did?";

pub struct SessionSurvey {
    acp_session_id: String,
    session_title: SharedString,
    /// Each TLX scale's answer, 0–10 (recorded ×10).
    tlx: [Option<u8>; 6],
    /// 1–7.
    agency: Option<u8>,
    opened_at: Instant,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for SessionSurvey {}
impl ModalView for SessionSurvey {}

impl Focusable for SessionSurvey {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Open the survey for the agent panel's current thread.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let thread = workspace
        .panel::<crate::AgentPanel>(cx)
        .and_then(|panel| panel.read(cx).active_agent_thread(cx));
    let Some(thread) = thread else {
        return;
    };
    let (acp_session_id, session_title) = {
        let thread = thread.read(cx);
        (
            thread.session_id().0.to_string(),
            thread
                .title()
                .unwrap_or_else(|| SharedString::new_static("This session")),
        )
    };
    workspace.toggle_modal(window, cx, |_, cx| SessionSurvey {
        acp_session_id,
        session_title,
        tlx: [None; 6],
        agency: None,
        opened_at: Instant::now(),
        focus_handle: cx.focus_handle(),
    });
}

impl SessionSurvey {
    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn complete(&self) -> bool {
        self.tlx.iter().all(Option::is_some) && self.agency.is_some()
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.complete() {
            return;
        }
        let mut scores = serde_json::Map::new();
        let mut total = 0u32;
        for ((key, ..), answer) in TLX_SCALES.iter().zip(self.tlx) {
            let score = u32::from(answer.unwrap_or_default()) * 10;
            total += score;
            scores.insert((*key).to_string(), score.into());
        }
        agent_research::record(
            &self.acp_session_id,
            "survey",
            serde_json::json!({
                "instrument": "NASA-TLX (raw, unweighted) + sense of agency",
                "tlx": scores,
                "tlxMean": f64::from(total) / TLX_SCALES.len() as f64,
                "senseOfAgency": self.agency,
                "secondsToComplete": self.opened_at.elapsed().as_secs(),
            }),
        );
        cx.emit(DismissEvent);
    }

    fn scale_row(
        &self,
        index: usize,
        label: &'static str,
        question: &'static str,
        ends: (&'static str, &'static str),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (points, selected) = if index < TLX_SCALES.len() {
            ((0u8..=10).collect::<Vec<_>>(), self.tlx[index])
        } else {
            ((1u8..=7).collect::<Vec<_>>(), self.agency)
        };
        v_flex()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small))
            .child(
                Label::new(question)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(ends.0)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .children(points.into_iter().map(|point| {
                        let shown = if index < TLX_SCALES.len() {
                            (u32::from(point) * 10).to_string()
                        } else {
                            point.to_string()
                        };
                        Button::new(("survey-point", index * 100 + usize::from(point)), shown)
                            .label_size(LabelSize::XSmall)
                            .style(if selected == Some(point) {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .toggle_state(selected == Some(point))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                match this.tlx.get_mut(index) {
                                    Some(slot) => *slot = Some(point),
                                    None => this.agency = Some(point),
                                }
                                cx.notify();
                            }))
                    }))
                    .child(
                        Label::new(ends.1)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }
}

impl Render for SessionSurvey {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let complete = self.complete();
        v_flex()
            .key_context("SessionSurvey")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(40.))
            .p_4()
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Headline::new("Rate this session").size(HeadlineSize::Small))
                    .child(
                        Label::new(format!(
                            "{} · saved with this session's research data",
                            self.session_title
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .children(
                TLX_SCALES
                    .iter()
                    .enumerate()
                    .map(|(index, (_, label, question, low, high))| {
                        self.scale_row(index, label, question, (low, high), cx)
                            .into_any_element()
                    })
                    .collect::<Vec<_>>(),
            )
            .child(self.scale_row(
                TLX_SCALES.len(),
                "Sense of agency",
                AGENCY_QUESTION,
                ("Not at all", "Completely"),
                cx,
            ))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("survey-cancel", "Not now")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("survey-submit", "Save rating")
                            .style(ButtonStyle::Filled)
                            .disabled(!complete)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}
