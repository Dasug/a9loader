//! The boot's end-of-run report line. [`result_line`] is the final count line
//! the engine logs once the whole boot is done.

/// The boot's final count line: memory effects applied/refused.
pub(super) fn result_line(applied: usize, refused: usize) -> String {
    format!("result: {applied} memory effect(s) applied, {refused} refused — game continues")
}
