//! Color table primitives; `.pal3` parsing will land after base rendering.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_has_zero_alpha() {
        assert_eq!(Rgba8::TRANSPARENT.a, 0);
    }
}
