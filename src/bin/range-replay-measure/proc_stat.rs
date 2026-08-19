//! Minimal Linux `/proc/self/stat` CPU accounting for one measured sample.

use std::num::ParseIntError;
use std::{fs, io};

use thiserror::Error;

/// Process CPU counters reported by Linux in clock ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProcessCpuTicks {
    user: u64,
    system: u64,
}

/// Non-negative CPU work between two process counter snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProcessCpuDelta {
    pub(super) user: u64,
    pub(super) system: u64,
}

impl ProcessCpuTicks {
    pub(super) fn read() -> Result<Self, ProcStatError> {
        let stat = fs::read_to_string("/proc/self/stat").map_err(ProcStatError::Read)?;
        parse_proc_stat(&stat)
    }

    pub(super) fn delta_to(self, end: Self) -> Result<ProcessCpuDelta, ProcStatError> {
        let user = end
            .user
            .checked_sub(self.user)
            .ok_or(ProcStatError::CounterRegression("user"))?;
        let system = end
            .system
            .checked_sub(self.system)
            .ok_or(ProcStatError::CounterRegression("system"))?;

        Ok(ProcessCpuDelta { user, system })
    }
}

impl ProcessCpuDelta {
    pub(super) fn nanoseconds(self, ticks_per_second: u64) -> Result<u128, ProcStatError> {
        if ticks_per_second == 0 {
            return Err(ProcStatError::ZeroTicksPerSecond);
        }

        u128::from(self.user)
            .checked_add(u128::from(self.system))
            .and_then(|ticks| ticks.checked_mul(1_000_000_000))
            .map(|scaled| scaled / u128::from(ticks_per_second))
            .ok_or(ProcStatError::CpuNanosecondsOverflow)
    }
}

/// Reason Linux process CPU accounting could not be read or interpreted.
#[derive(Debug, Error)]
pub(super) enum ProcStatError {
    #[error("cannot read /proc/self/stat")]
    Read(#[source] io::Error),
    #[error("/proc/self/stat has no closing command delimiter")]
    MissingCommandDelimiter,
    #[error("/proc/self/stat omits field {0}")]
    MissingField(&'static str),
    #[error("/proc/self/stat field {field} is not an unsigned integer")]
    InvalidField {
        field: &'static str,
        #[source]
        source: ParseIntError,
    },
    #[error("process {0} CPU counter moved backwards")]
    CounterRegression(&'static str),
    #[error("clock ticks per second must be greater than zero")]
    ZeroTicksPerSecond,
    #[error("CPU nanosecond conversion overflowed")]
    CpuNanosecondsOverflow,
}

fn parse_proc_stat(stat: &str) -> Result<ProcessCpuTicks, ProcStatError> {
    let command_end = stat
        .rfind(')')
        .ok_or(ProcStatError::MissingCommandDelimiter)?;
    let fields = stat
        .get(command_end.saturating_add(1)..)
        .ok_or(ProcStatError::MissingCommandDelimiter)?;
    let mut fields = fields.split_whitespace();

    let user = nth_field(&mut fields, 11, "utime")?;
    let system = nth_field(&mut fields, 0, "stime")?;

    Ok(ProcessCpuTicks { user, system })
}

fn nth_field<'field>(
    fields: &mut impl Iterator<Item = &'field str>,
    skip: usize,
    name: &'static str,
) -> Result<u64, ProcStatError> {
    let value = fields.nth(skip).ok_or(ProcStatError::MissingField(name))?;

    value.parse().map_err(|source| ProcStatError::InvalidField {
        field: name,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::{ProcStatError, ProcessCpuTicks, parse_proc_stat};

    #[test]
    fn parser_uses_fields_fourteen_and_fifteen_after_a_spaced_command() {
        let parsed = parse_proc_stat(
            "123 (range replay) R 1 2 3 4 5 6 7 8 9 10 111 222 13 14 15 16 17 18 19",
        )
        .expect("fixture includes process CPU fields");

        assert_eq!(
            parsed,
            ProcessCpuTicks {
                user: 111,
                system: 222,
            }
        );
    }

    #[test]
    fn parser_rejects_a_missing_command_delimiter() {
        assert!(matches!(
            parse_proc_stat("123 range-replay R 1 2 3"),
            Err(ProcStatError::MissingCommandDelimiter)
        ));
    }

    #[test]
    fn cpu_delta_rejects_regressing_counters() {
        let start = ProcessCpuTicks {
            user: 10,
            system: 20,
        };
        let end = ProcessCpuTicks {
            user: 9,
            system: 21,
        };

        assert!(matches!(
            start.delta_to(end),
            Err(ProcStatError::CounterRegression("user"))
        ));
    }

    #[test]
    fn cpu_nanoseconds_preserve_user_and_system_ticks() {
        let delta = ProcessCpuTicks {
            user: 10,
            system: 20,
        }
        .delta_to(ProcessCpuTicks {
            user: 13,
            system: 24,
        })
        .expect("counters increase");

        assert_eq!(delta.user, 3);
        assert_eq!(delta.system, 4);
        assert_eq!(
            delta.nanoseconds(100).expect("100 Hz is non-zero"),
            70_000_000
        );
    }
}
