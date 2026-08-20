use std::time::{Duration, Instant};

use super::terminal::TerminalCursorState;

pub(crate) const CURSOR_POSITION_SETTLE: Duration = Duration::from_millis(20);
const CURSOR_POSITION_MAX_HOLD: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub(crate) struct DecscusrTracker {
    state: DecscusrParseState,
    cursor_shape_overridden: bool,
}

#[derive(Debug, Default)]
enum DecscusrParseState {
    #[default]
    Ground,
    Escape,
    Csi {
        first_param: Option<u16>,
        collecting_first_param: bool,
        has_space_intermediate: bool,
    },
}

impl DecscusrTracker {
    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.observe_byte(byte);
        }
    }

    fn observe_byte(&mut self, byte: u8) {
        match &mut self.state {
            DecscusrParseState::Ground => {
                if byte == 0x1b {
                    self.state = DecscusrParseState::Escape;
                }
            }
            DecscusrParseState::Escape => {
                self.state = if byte == b'[' {
                    DecscusrParseState::Csi {
                        first_param: None,
                        collecting_first_param: true,
                        has_space_intermediate: false,
                    }
                } else if byte == 0x1b {
                    DecscusrParseState::Escape
                } else {
                    DecscusrParseState::Ground
                };
            }
            DecscusrParseState::Csi {
                first_param,
                collecting_first_param,
                has_space_intermediate,
            } => {
                if byte == 0x1b {
                    self.state = DecscusrParseState::Escape;
                } else if byte.is_ascii_digit() && *collecting_first_param {
                    let digit = u16::from(byte - b'0');
                    *first_param = Some(first_param.unwrap_or(0).saturating_mul(10) + digit);
                } else if byte == b';' || byte == b':' {
                    *collecting_first_param = false;
                } else if byte == b' ' {
                    *has_space_intermediate = true;
                    *collecting_first_param = false;
                } else if (0x40..=0x7e).contains(&byte) {
                    if byte == b'q' && *has_space_intermediate {
                        let param = first_param.unwrap_or(0);
                        if param <= 6 {
                            self.cursor_shape_overridden = param != 0;
                        }
                    }
                    self.state = DecscusrParseState::Ground;
                } else if !(0x20..=0x3f).contains(&byte) {
                    self.state = DecscusrParseState::Ground;
                }
            }
        }
    }

    pub(crate) fn cursor_shape_overridden(&self) -> bool {
        self.cursor_shape_overridden
    }
}

/// How a child's cursor arrived at the position it currently holds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorPlacement {
    /// An explicit positioning sequence put it there.
    #[default]
    Positioned,
    /// It is wherever printing left it, which for a full redraw is the cell the
    /// frame painted last rather than anywhere the child means the cursor to be.
    Printed,
}

/// Tracks whether the last thing to move a child's cursor was an explicit
/// positioning sequence or the side effect of printing.
///
/// Applications that bracket a frame in synchronized output may close the
/// bracket before placing their cursor, so the position at the close is only
/// trustworthy when the frame ended on a positioning sequence. Position and
/// timing alone cannot tell the two apart.
#[derive(Debug, Default)]
pub(crate) struct CursorPlacementTracker {
    state: PlacementParseState,
    placement: CursorPlacement,
}

#[derive(Debug, Default)]
enum PlacementParseState {
    #[default]
    Ground,
    Escape,
    /// An escape sequence with an intermediate byte, such as charset selection.
    EscapeIntermediate,
    Csi,
    /// An OSC, DCS, APC or PM payload, whose text must not count as printing.
    StringPayload,
    StringPayloadEscape,
}

/// CSI final bytes that move the cursor to a position of the child's choosing.
///
/// `s` and `u` are deliberately absent: they collide with the kitty keyboard
/// protocol, and missing a placement only holds the cursor a little longer,
/// while a false placement puts it on the wrong cell.
const CURSOR_POSITIONING_FINALS: &[u8] = b"ABCDEFGHIZdefa`";

/// CSI final bytes that erase or edit at the cursor.
///
/// An application that positions the cursor in order to clear or edit that spot
/// has not chosen it as the cursor's home: this is how a redraw clears the tail
/// of a row, and the frame's real cursor placement is emitted after all such
/// edits. Treating what those leave behind as incidental is what keeps the
/// cursor off the row a frame happened to clear last.
const CURSOR_INCIDENTAL_FINALS: &[u8] = b"JKLMPX@";

impl CursorPlacementTracker {
    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.observe_byte(byte);
        }
    }

    pub(crate) fn placement(&self) -> CursorPlacement {
        self.placement
    }

    fn observe_byte(&mut self, byte: u8) {
        match self.state {
            PlacementParseState::Ground => match byte {
                0x1b => self.state = PlacementParseState::Escape,
                // Printing, and the control codes that only advance or wrap,
                // leave the cursor wherever the text ended.
                0x08..=0x0d => self.placement = CursorPlacement::Printed,
                byte if byte >= 0x20 => self.placement = CursorPlacement::Printed,
                _ => {}
            },
            PlacementParseState::Escape => match byte {
                b'[' => self.state = PlacementParseState::Csi,
                b']' | b'P' | b'X' | b'^' | b'_' => self.state = PlacementParseState::StringPayload,
                // DECRC restores a saved cursor position.
                b'8' => {
                    self.placement = CursorPlacement::Positioned;
                    self.state = PlacementParseState::Ground;
                }
                0x1b => {}
                b'(' | b')' | b'*' | b'+' | b'%' | b'#' | b' ' => {
                    self.state = PlacementParseState::EscapeIntermediate
                }
                _ => self.state = PlacementParseState::Ground,
            },
            PlacementParseState::EscapeIntermediate => self.state = PlacementParseState::Ground,
            PlacementParseState::Csi => {
                if byte == 0x1b {
                    self.state = PlacementParseState::Escape;
                } else if (0x40..=0x7e).contains(&byte) {
                    if CURSOR_POSITIONING_FINALS.contains(&byte) {
                        self.placement = CursorPlacement::Positioned;
                    } else if CURSOR_INCIDENTAL_FINALS.contains(&byte) {
                        self.placement = CursorPlacement::Printed;
                    }
                    self.state = PlacementParseState::Ground;
                }
            }
            PlacementParseState::StringPayload => match byte {
                0x1b => self.state = PlacementParseState::StringPayloadEscape,
                0x07 => self.state = PlacementParseState::Ground,
                _ => {}
            },
            PlacementParseState::StringPayloadEscape => {
                self.state = if byte == 0x1b {
                    PlacementParseState::StringPayloadEscape
                } else {
                    PlacementParseState::Ground
                };
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct CursorPositionSettleState {
    settled: Option<TerminalCursorState>,
    candidate: Option<TerminalCursorState>,
    pending_since: Option<Instant>,
    pending_first: Option<Instant>,
    last_observed: Option<Instant>,
    synchronized_frame_open: bool,
    candidate_awaiting_placement: bool,
}

impl CursorPositionSettleState {
    fn clear_pending(&mut self) {
        self.candidate = None;
        self.pending_since = None;
        self.pending_first = None;
        self.candidate_awaiting_placement = false;
    }

    /// Note that the child has output pending inside its own synchronized output
    /// block. Nothing renders and no cursor is published until the block closes,
    /// so this is not an observation: it only marks the next observation as the
    /// one that closes a frame.
    pub(crate) fn observe_synchronized(&mut self) {
        self.synchronized_frame_open = true;
    }

    /// Convenience for the tests that do not exercise placement handling.
    #[cfg(test)]
    fn observe(&mut self, current: Option<TerminalCursorState>, now: Instant) {
        self.observe_with_placement(current, now, CursorPlacement::Positioned);
    }

    pub(crate) fn observe_with_placement(
        &mut self,
        current: Option<TerminalCursorState>,
        now: Instant,
        placement: CursorPlacement,
    ) {
        let closing_frame = std::mem::take(&mut self.synchronized_frame_open);
        let previous_observation = self.last_observed.replace(now);
        let Some(current) = current else {
            self.settled = None;
            self.clear_pending();
            return;
        };
        if !current.visible {
            self.settled = Some(current);
            self.clear_pending();
            return;
        }

        // A quiet gap means the previous write burst finished, so its last
        // position was the one the child meant. Commit it and start fresh.
        // Without this the max hold below eventually fires on the *first*
        // position of the next burst, which is exactly the mid-redraw position
        // we are trying not to expose, and settling on it makes every later
        // redraw publish it with no hold at all.
        if let (Some(candidate), Some(previous), false) = (
            self.candidate,
            previous_observation,
            self.candidate_awaiting_placement,
        ) {
            if now.duration_since(previous) >= CURSOR_POSITION_SETTLE {
                self.settled = Some(candidate);
                self.clear_pending();
            }
        }

        let Some(settled) = self.settled else {
            self.settled = Some(current);
            self.clear_pending();
            return;
        };
        if same_cursor_position(settled, current) && settled.visible {
            self.settled = Some(current);
            self.clear_pending();
            return;
        }

        // The child closed a synchronized frame that ended on printed text, so
        // this is the cell the frame painted last and the real placement arrives
        // in a later write. Hold the previous position until it does instead of
        // treating a pause as proof that the frame is finished.
        if closing_frame && placement == CursorPlacement::Printed {
            self.candidate = Some(current);
            self.pending_since = Some(now);
            self.pending_first.get_or_insert(now);
            self.candidate_awaiting_placement = true;
            return;
        }

        let Some(candidate) = self.candidate else {
            self.candidate = Some(current);
            self.pending_since = Some(now);
            self.pending_first = Some(now);
            self.candidate_awaiting_placement = false;
            return;
        };

        let pending_since = self.pending_since.unwrap_or(now);
        let pending_first = self.pending_first.unwrap_or(pending_since);
        if now.duration_since(pending_first) >= CURSOR_POSITION_MAX_HOLD {
            self.settled = Some(current);
            self.clear_pending();
        } else if same_cursor_position(candidate, current) {
            if now.duration_since(pending_since) >= CURSOR_POSITION_SETTLE {
                self.settled = Some(current);
                self.clear_pending();
            } else {
                self.candidate = Some(current);
            }
        } else {
            // A different position means the child is still moving the cursor, so
            // restart the quiet window and let the max hold bound the total wait.
            // Without this the first change in a burst opens a window that expires
            // mid-burst, after which every render publishes whatever was seen last.
            self.candidate = Some(current);
            self.pending_since = Some(now);
            self.candidate_awaiting_placement = false;
        }
    }

    pub(crate) fn reported_cursor(
        &self,
        current: Option<TerminalCursorState>,
        now: Instant,
    ) -> Option<TerminalCursorState> {
        let current = current?;
        let Some(candidate) = self.candidate else {
            return Some(current);
        };
        let pending_since = self.pending_since.unwrap_or(now);
        let hold = if self.candidate_awaiting_placement {
            CURSOR_POSITION_MAX_HOLD
        } else {
            CURSOR_POSITION_SETTLE
        };
        if now.duration_since(pending_since) >= hold {
            return Some(TerminalCursorState {
                visible: current.visible && candidate.visible,
                shape: current.shape,
                ..candidate
            });
        }
        self.settled
            .map(|settled| TerminalCursorState {
                visible: current.visible && settled.visible,
                shape: current.shape,
                ..settled
            })
            .or(Some(TerminalCursorState {
                visible: false,
                shape: current.shape,
                ..candidate
            }))
    }

    pub(crate) fn pending(&self) -> bool {
        self.candidate.is_some()
    }
}

fn same_cursor_position(left: TerminalCursorState, right: TerminalCursorState) -> bool {
    left.x == right.x && left.y == right.y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(x: u16, y: u16, visible: bool, shape: u8) -> TerminalCursorState {
        TerminalCursorState {
            x,
            y,
            visible,
            shape,
        }
    }

    #[test]
    fn cursor_settle_holds_position_change_until_quiet_window() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);
        settle.observe(Some(cursor(20, 5, true, 0)), now + Duration::from_millis(1));

        let reported = settle
            .reported_cursor(Some(cursor(20, 5, true, 0)), now + Duration::from_millis(2))
            .unwrap();

        assert_eq!((reported.x, reported.y), (1, 0));
    }

    #[test]
    fn cursor_settle_adopts_position_change_after_quiet_window() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);
        settle.observe(Some(cursor(2, 0, true, 0)), now + Duration::from_millis(1));

        let reported = settle
            .reported_cursor(
                Some(cursor(2, 0, true, 0)),
                now + CURSOR_POSITION_SETTLE + Duration::from_millis(1),
            )
            .unwrap();

        assert_eq!((reported.x, reported.y), (2, 0));
    }

    #[test]
    fn cursor_settle_caps_continuous_position_changes_from_first_pending_time() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);

        // Churn arriving faster than the settle window never lets the burst
        // end, so only the max hold can release the held position.
        let mut at = now;
        let mut last = 1u16;
        while at.duration_since(now) < CURSOR_POSITION_MAX_HOLD + Duration::from_millis(10) {
            at += Duration::from_millis(10);
            last += 1;
            settle.observe(Some(cursor(last, 0, true, 0)), at);
        }

        assert!(!settle.pending());
        assert_eq!(
            settle.reported_cursor(
                Some(cursor(last, 0, true, 0)),
                at + Duration::from_millis(1)
            ),
            Some(cursor(last, 0, true, 0))
        );
    }

    #[test]
    fn cursor_settle_commits_candidate_when_the_write_burst_ends() {
        // Two redraws separated by an idle gap. The second redraw must not be
        // able to settle on its own mid-redraw position.
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        let shell = cursor(2, 5, true, 0);
        let caret = cursor(7, 34, true, 0);
        let footer = cursor(33, 36, true, 0);

        settle.observe(Some(shell), now);
        settle.observe(Some(footer), now + Duration::from_millis(10));
        settle.observe(Some(caret), now + Duration::from_millis(18));

        settle.observe(Some(footer), now + Duration::from_millis(218));
        let reported = settle
            .reported_cursor(Some(footer), now + Duration::from_millis(220))
            .expect("cursor");

        assert_eq!((reported.x, reported.y), (caret.x, caret.y));
    }

    #[test]
    fn cursor_settle_never_reports_a_mid_redraw_paint_position() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(7, 19, true, 0)), now);

        // A redraw leaves the cursor on the last repainted row before the child
        // places it back on its input line.
        settle.observe(Some(cursor(5, 23, true, 0)), now + Duration::from_millis(1));
        assert_eq!(
            settle.reported_cursor(Some(cursor(5, 23, true, 0)), now + Duration::from_millis(2)),
            Some(cursor(7, 19, true, 0))
        );

        settle.observe(Some(cursor(8, 19, true, 0)), now + Duration::from_millis(3));
        assert_eq!(
            settle.reported_cursor(Some(cursor(8, 19, true, 0)), now + Duration::from_millis(4)),
            Some(cursor(7, 19, true, 0))
        );
        assert_eq!(
            settle.reported_cursor(
                Some(cursor(8, 19, true, 0)),
                now + Duration::from_millis(3) + CURSOR_POSITION_SETTLE + Duration::from_millis(1),
            ),
            Some(cursor(8, 19, true, 0))
        );
    }

    #[test]
    fn cursor_placement_tracker_separates_positioning_from_printing() {
        let mut tracker = CursorPlacementTracker::default();
        assert_eq!(tracker.placement(), CursorPlacement::Positioned);

        tracker.observe(b"Ready");
        assert_eq!(tracker.placement(), CursorPlacement::Printed);

        tracker.observe(b"\x1b[24;1H");
        assert_eq!(tracker.placement(), CursorPlacement::Positioned);

        tracker.observe(b"\x1b[24;1HReady");
        assert_eq!(tracker.placement(), CursorPlacement::Printed);

        // A title update after the placement is not printing.
        tracker.observe(b"\x1b[12;9H\x1b]0;codex\x07");
        assert_eq!(tracker.placement(), CursorPlacement::Positioned);

        // Styling and mode changes do not move the cursor.
        tracker.observe(b"\x1b[24;1H\x1b[1;32m\x1b[?25h");
        assert_eq!(tracker.placement(), CursorPlacement::Positioned);

        // Positioning in order to erase is not a placement. This is the measured
        // tail of a real agent frame: it clears the end of its status row, shows
        // the cursor and closes the block, and places its caret only afterwards.
        tracker.observe(b"\x1b[m\x1b[14;66H\x1b[K\x1b[?25h\x1b[?2026l");
        assert_eq!(tracker.placement(), CursorPlacement::Printed);

        // The placement that follows the erase is the real one.
        tracker.observe(b"\x1b[12;9H\x1b[?25h");
        assert_eq!(tracker.placement(), CursorPlacement::Positioned);

        // A sequence split across read batches still parses.
        tracker.observe(b"text");
        tracker.observe(b"\x1b[9");
        tracker.observe(b";4H");
        assert_eq!(tracker.placement(), CursorPlacement::Positioned);

        // Wrapping and line feeds leave the cursor wherever the text ended.
        tracker.observe(b"\r\n");
        assert_eq!(tracker.placement(), CursorPlacement::Printed);
    }

    #[test]
    fn cursor_settle_holds_a_frame_that_closed_on_printed_text() {
        // Measured shape from a real agent pane: the child paints inside its
        // synchronized output block, closes the block with the cursor still on the
        // row it painted last, and places its caret only afterwards - 24ms later,
        // which is past the settle window.
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        let caret = cursor(58, 11, true, 0);
        let painted = cursor(65, 13, true, 0);
        let placed = cursor(59, 11, true, 0);

        settle.observe(Some(caret), now);
        settle.observe_synchronized();
        settle.observe_synchronized();
        settle.observe_with_placement(
            Some(painted),
            now + Duration::from_millis(1),
            CursorPlacement::Printed,
        );

        for after in [2, 10, 21, 23] {
            assert_eq!(
                settle
                    .reported_cursor(Some(painted), now + Duration::from_millis(after))
                    .map(|reported| (reported.x, reported.y)),
                Some((caret.x, caret.y)),
                "reported the painted row {after}ms after the frame closed"
            );
        }

        settle.observe_with_placement(
            Some(placed),
            now + Duration::from_millis(25),
            CursorPlacement::Positioned,
        );
        assert_eq!(
            settle
                .reported_cursor(Some(placed), now + Duration::from_millis(26))
                .map(|reported| (reported.x, reported.y)),
            Some((caret.x, caret.y)),
            "the gap before the placement must not settle the painted row"
        );
        assert_eq!(
            settle
                .reported_cursor(Some(placed), now + Duration::from_millis(46))
                .map(|reported| (reported.x, reported.y)),
            Some((placed.x, placed.y))
        );
    }

    #[test]
    fn cursor_settle_adopts_a_frame_that_closed_on_a_placement() {
        // The same frame shape, except the child placed its cursor inside the
        // block before closing it. That position is real, so it must not be held
        // any longer than an ordinary position change or the cursor trails every
        // keystroke instead of following it.
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        let previous = cursor(23, 11, true, 0);
        let placed = cursor(24, 11, true, 0);

        settle.observe(Some(previous), now);
        settle.observe_synchronized();
        settle.observe_with_placement(
            Some(placed),
            now + Duration::from_millis(1),
            CursorPlacement::Positioned,
        );

        assert_eq!(
            settle
                .reported_cursor(Some(placed), now + Duration::from_millis(2))
                .map(|reported| (reported.x, reported.y)),
            Some((previous.x, previous.y))
        );
        assert_eq!(
            settle
                .reported_cursor(
                    Some(placed),
                    now + CURSOR_POSITION_SETTLE + Duration::from_millis(2)
                )
                .map(|reported| (reported.x, reported.y)),
            Some((placed.x, placed.y)),
            "a placed cursor must be adopted on the ordinary settle schedule"
        );
    }

    #[test]
    fn cursor_settle_keeps_render_read_pure() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);
        settle.observe(Some(cursor(2, 0, true, 0)), now + Duration::from_millis(1));

        assert!(settle.pending());
        let _ = settle.reported_cursor(
            Some(cursor(2, 0, true, 0)),
            now + CURSOR_POSITION_SETTLE + Duration::from_millis(1),
        );

        assert!(settle.pending());
    }

    #[test]
    fn cursor_settle_passes_shape_through_while_position_is_held() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 2)), now);
        settle.observe(Some(cursor(2, 0, true, 6)), now + Duration::from_millis(1));

        let reported = settle
            .reported_cursor(Some(cursor(2, 0, true, 6)), now + Duration::from_millis(2))
            .unwrap();

        assert_eq!((reported.x, reported.y, reported.shape), (1, 0, 6));
    }

    #[test]
    fn cursor_settle_passes_shape_through_after_quiet_window() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 2)), now);
        settle.observe(Some(cursor(2, 0, true, 2)), now + Duration::from_millis(1));

        let reported = settle
            .reported_cursor(
                Some(cursor(2, 0, true, 6)),
                now + CURSOR_POSITION_SETTLE + Duration::from_millis(1),
            )
            .unwrap();

        assert_eq!((reported.x, reported.y, reported.shape), (2, 0, 6));
    }

    #[test]
    fn cursor_settle_hides_immediately_and_waits_to_reveal() {
        let now = Instant::now();
        let mut settle = CursorPositionSettleState::default();
        settle.observe(Some(cursor(1, 0, true, 0)), now);
        settle.observe(Some(cursor(1, 0, false, 0)), now + Duration::from_millis(1));

        assert_eq!(
            settle.reported_cursor(Some(cursor(1, 0, false, 0)), now + Duration::from_millis(2)),
            Some(cursor(1, 0, false, 0))
        );

        settle.observe(Some(cursor(1, 0, true, 0)), now + Duration::from_millis(3));
        assert_eq!(
            settle.reported_cursor(Some(cursor(1, 0, true, 0)), now + Duration::from_millis(4)),
            Some(cursor(1, 0, false, 0))
        );
    }
}
