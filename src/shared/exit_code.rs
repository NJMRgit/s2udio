use std::ops::{Add, AddAssign, BitAnd, BitAndAssign, BitOr, BitOrAssign};
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ExitCode(u8);
impl BitOr for ExitCode {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        ExitCode(self.0 | rhs.0)
    }
}
impl BitOrAssign for ExitCode {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl BitAnd for ExitCode {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        ExitCode(self.0 & rhs.0)
    }
}
impl BitAndAssign for ExitCode {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}
impl Add for ExitCode {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        ExitCode(self.0 + rhs.0)
    }
}
impl AddAssign for ExitCode {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}
impl From<u8> for ExitCode {
    fn from(value: u8) -> Self {
        ExitCode(value)
    }
}
impl From<ExitCode> for i32 {
    fn from(value: ExitCode) -> Self {
        value.0 as i32
    }
}
