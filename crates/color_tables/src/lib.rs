//! Fast color table parsing and sampling for radar renderers.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

const KNOT_TO_MPS: f32 = 0.514_444;
const MPH_TO_MPS: f32 = 0.447_04;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }

    fn lerp(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        Self {
            r: lerp_u8(self.r, other.r, amount),
            g: lerp_u8(self.g, other.g, amount),
            b: lerp_u8(self.b, other.b, amount),
            a: lerp_u8(self.a, other.a, amount),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColorTableFamily {
    Reflectivity,
    Velocity,
    SpectrumWidth,
    Generic,
}

impl ColorTableFamily {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reflectivity => "Reflectivity",
            Self::Velocity => "Velocity / SRV",
            Self::SpectrumWidth => "Spectrum Width",
            Self::Generic => "Other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorStop {
    pub value: f32,
    pub color: Rgba8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorTable {
    name: String,
    product: Option<String>,
    units: Option<String>,
    range_folded: Rgba8,
    sample_mode: SampleMode,
    stops: Vec<ColorStop>,
}

impl ColorTable {
    pub fn new(name: impl Into<String>, stops: Vec<ColorStop>) -> Result<Self, ColorTableError> {
        Self::from_parts(
            name.into(),
            None,
            None,
            default_range_folded_color(),
            SampleMode::Interpolated,
            stops,
        )
    }

    pub fn new_stepped(
        name: impl Into<String>,
        stops: Vec<ColorStop>,
    ) -> Result<Self, ColorTableError> {
        Self::from_parts(
            name.into(),
            None,
            None,
            default_range_folded_color(),
            SampleMode::Stepped,
            stops,
        )
    }

    pub fn parse(name: impl Into<String>, text: &str) -> Result<Self, ColorTableError> {
        Self::parse_with_default_mode(name, text, SampleMode::Interpolated)
    }

    pub fn parse_with_default_mode(
        name: impl Into<String>,
        text: &str,
        default_sample_mode: SampleMode,
    ) -> Result<Self, ColorTableError> {
        let name = name.into();
        let mut product = None;
        let mut units = None;
        let mut scale = None;
        let mut range_folded = default_range_folded_color();
        let mut sample_mode = default_sample_mode;
        let mut stops = Vec::new();

        for (line_index, original_line) in text.lines().enumerate() {
            let line_number = line_index + 1;
            let line = normalize_line(original_line);
            let line = line.trim();
            if line.is_empty()
                || line.starts_with(';')
                || line.starts_with('#')
                || line.starts_with("$$")
            {
                continue;
            }

            let Some((raw_key, raw_value)) = split_key_value(line) else {
                continue;
            };
            let key = normalize_key(raw_key);
            let value = raw_value.trim();

            match key.as_str() {
                "product" => product = non_empty(value),
                "units" => units = non_empty(value),
                "scale" => scale = parse_positive_f32(value),
                "step" => sample_mode = SampleMode::Stepped,
                "mode" | "samplemode" | "interpolate" | "interpolation" | "smooth" => {
                    if let Some(parsed_mode) = parse_sample_mode(value) {
                        sample_mode = parsed_mode;
                    }
                }
                "rf" | "rangefolded" | "rangefoldedcolor" => {
                    range_folded = parse_color_only(value, line_number)?;
                }
                "color" | "color4" | "solidcolor" | "solidcolor4" => {
                    stops.push(parse_color_stop(value, key.ends_with('4'), line_number)?);
                }
                _ => {}
            }
        }

        let unit_scale = scale
            .map(|scale| 1.0 / scale)
            .or_else(|| units.as_deref().map(unit_value_to_mps_scale))
            .unwrap_or(1.0);
        if unit_scale != 1.0 {
            for stop in &mut stops {
                stop.value *= unit_scale;
            }
        }

        Self::from_parts(name, product, units, range_folded, sample_mode, stops)
    }

    pub fn parse_stepped(name: impl Into<String>, text: &str) -> Result<Self, ColorTableError> {
        Self::parse_with_default_mode(name, text, SampleMode::Stepped)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn product(&self) -> Option<&str> {
        self.product.as_deref()
    }

    pub fn units(&self) -> Option<&str> {
        self.units.as_deref()
    }

    pub fn stops(&self) -> &[ColorStop] {
        &self.stops
    }

    pub fn interpolates(&self) -> bool {
        self.sample_mode == SampleMode::Interpolated
    }

    pub fn sample_mode_label(&self) -> &'static str {
        self.sample_mode.label()
    }

    pub fn sample(&self, value: f32) -> Rgba8 {
        if !value.is_finite() {
            return Rgba8::TRANSPARENT;
        }
        let Some(first) = self.stops.first() else {
            return Rgba8::TRANSPARENT;
        };
        if value <= first.value {
            return first.color;
        }
        let index = self.stops.partition_point(|stop| stop.value < value);
        if index >= self.stops.len() {
            return self
                .stops
                .last()
                .map(|stop| stop.color)
                .unwrap_or(Rgba8::TRANSPARENT);
        }
        let right = self.stops[index];
        if value == right.value {
            return right.color;
        }
        let left = self.stops[index - 1];
        match self.sample_mode {
            SampleMode::Interpolated => {
                let span = (right.value - left.value).max(f32::EPSILON);
                left.color.lerp(right.color, (value - left.value) / span)
            }
            SampleMode::Stepped => left.color,
        }
    }

    pub fn color_for_value(&self, value: f32) -> [u8; 4] {
        self.sample(value).to_array()
    }

    pub fn range_folded_color(&self) -> [u8; 4] {
        self.range_folded.to_array()
    }

    pub fn range_folded_rgba(&self) -> Rgba8 {
        self.range_folded
    }

    pub fn signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.name.hash(&mut hasher);
        self.product.hash(&mut hasher);
        self.units.hash(&mut hasher);
        self.range_folded.hash(&mut hasher);
        self.sample_mode.hash(&mut hasher);
        self.stops.len().hash(&mut hasher);
        for stop in &self.stops {
            stop.value.to_bits().hash(&mut hasher);
            stop.color.hash(&mut hasher);
        }
        hasher.finish()
    }

    fn from_parts(
        name: String,
        product: Option<String>,
        units: Option<String>,
        range_folded: Rgba8,
        sample_mode: SampleMode,
        mut stops: Vec<ColorStop>,
    ) -> Result<Self, ColorTableError> {
        stops.retain(|stop| stop.value.is_finite());
        stops.sort_by(|left, right| left.value.total_cmp(&right.value));
        stops.dedup_by(|left, right| {
            if left.value.to_bits() == right.value.to_bits() {
                *left = *right;
                true
            } else {
                false
            }
        });

        if stops.len() < 2 {
            return Err(ColorTableError::NotEnoughStops);
        }

        Ok(Self {
            name,
            product,
            units,
            range_folded,
            sample_mode,
            stops,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SampleMode {
    Interpolated,
    Stepped,
}

impl SampleMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interpolated => "interpolated",
            Self::Stepped => "stepped",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorTableSet {
    reflectivity: ColorTable,
    velocity: ColorTable,
    spectrum_width: ColorTable,
    generic: ColorTable,
}

impl ColorTableSet {
    pub fn for_family(&self, family: ColorTableFamily) -> &ColorTable {
        match family {
            ColorTableFamily::Reflectivity => &self.reflectivity,
            ColorTableFamily::Velocity => &self.velocity,
            ColorTableFamily::SpectrumWidth => &self.spectrum_width,
            ColorTableFamily::Generic => &self.generic,
        }
    }

    pub fn set_family(&mut self, family: ColorTableFamily, table: ColorTable) {
        match family {
            ColorTableFamily::Reflectivity => self.reflectivity = table,
            ColorTableFamily::Velocity => self.velocity = table,
            ColorTableFamily::SpectrumWidth => self.spectrum_width = table,
            ColorTableFamily::Generic => self.generic = table,
        }
    }

    pub fn signature_for_family(&self, family: ColorTableFamily) -> u64 {
        self.for_family(family).signature()
    }
}

impl Default for ColorTableSet {
    fn default() -> Self {
        Self {
            reflectivity: builtin_reflectivity_table(),
            velocity: builtin_velocity_table(),
            spectrum_width: builtin_spectrum_width_table(),
            generic: builtin_generic_table(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ColorTableError {
    InvalidColor { line: usize, reason: &'static str },
    NotEnoughStops,
}

impl fmt::Display for ColorTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidColor { line, reason } => {
                write!(formatter, "invalid color table line {line}: {reason}")
            }
            Self::NotEnoughStops => write!(formatter, "color table needs at least two color stops"),
        }
    }
}

impl std::error::Error for ColorTableError {}

pub fn builtin_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("WxTools RadarScope BR", RADARSCOPE_REFLECTIVITY_TABLE)
        .expect("built-in reflectivity color table is valid")
}

pub fn builtin_velocity_table() -> ColorTable {
    analyst_velocity_table()
}

pub fn vortex_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("WxTools Vortex Velo", VORTEX_VELO_TABLE)
        .expect("built-in velocity color table is valid")
}

pub fn builtin_tables_for_family(family: ColorTableFamily) -> Vec<ColorTable> {
    match family {
        ColorTableFamily::Reflectivity => vec![
            builtin_reflectivity_table(),
            analyst_reflectivity_table(),
            nws_reflectivity_table(),
        ],
        ColorTableFamily::Velocity => vec![
            builtin_velocity_table(),
            vortex_velocity_table(),
            nws_velocity_table(),
        ],
        ColorTableFamily::SpectrumWidth => vec![builtin_spectrum_width_table()],
        ColorTableFamily::Generic => vec![builtin_generic_table()],
    }
}

pub fn analyst_reflectivity_table() -> ColorTable {
    ColorTable::new_stepped(
        "Analyst High Contrast REF",
        vec![
            stop(-10.0, 5, 8, 18),
            stop(0.0, 18, 36, 76),
            stop(7.5, 23, 92, 157),
            stop(15.0, 26, 158, 191),
            stop(22.5, 17, 146, 62),
            stop(30.0, 84, 188, 54),
            stop(37.5, 242, 216, 47),
            stop(45.0, 239, 120, 34),
            stop(52.5, 221, 42, 38),
            stop(60.0, 174, 32, 112),
            stop(67.5, 214, 76, 218),
            stop(75.0, 245, 245, 245),
        ],
    )
    .expect("built-in analyst reflectivity color table is valid")
}

pub fn nws_reflectivity_table() -> ColorTable {
    ColorTable::new_stepped(
        "NWS Classic REF",
        vec![
            stop(5.0, 4, 233, 231),
            stop(10.0, 1, 159, 244),
            stop(15.0, 3, 0, 244),
            stop(20.0, 2, 253, 2),
            stop(25.0, 1, 197, 1),
            stop(30.0, 0, 142, 0),
            stop(35.0, 253, 248, 2),
            stop(40.0, 229, 188, 0),
            stop(45.0, 253, 149, 0),
            stop(50.0, 253, 0, 0),
            stop(55.0, 212, 0, 0),
            stop(60.0, 188, 0, 0),
            stop(65.0, 248, 0, 253),
            stop(70.0, 152, 84, 198),
            stop(75.0, 255, 255, 255),
        ],
    )
    .expect("built-in nws reflectivity color table is valid")
}

pub fn analyst_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Pro VEL", ANALYST_PRO_VELOCITY_TABLE)
        .expect("built-in analyst velocity color table is valid")
}

pub fn nws_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("NWS Classic VEL", NWS_VELOCITY_TABLE)
        .expect("built-in nws velocity color table is valid")
}

pub fn builtin_spectrum_width_table() -> ColorTable {
    ColorTable::new(
        "Analyst Spectrum Width",
        vec![
            stop(0.0, 9, 20, 32),
            stop(1.0, 24, 52, 100),
            stop(2.0, 22, 102, 172),
            stop(3.0, 18, 152, 180),
            stop(4.0, 36, 174, 98),
            stop(5.5, 160, 188, 58),
            stop(7.0, 232, 190, 54),
            stop(9.0, 238, 112, 42),
            stop(12.0, 216, 44, 50),
            stop(16.0, 160, 36, 136),
            stop(24.0, 235, 235, 235),
        ],
    )
    .expect("built-in spectrum width color table is valid")
}

pub fn builtin_generic_table() -> ColorTable {
    ColorTable::new(
        "Analyst Generic",
        vec![
            stop(0.0, 34, 40, 64),
            stop(10.0, 34, 82, 130),
            stop(25.0, 34, 132, 172),
            stop(40.0, 58, 166, 140),
            stop(55.0, 116, 180, 92),
            stop(70.0, 218, 188, 74),
            stop(85.0, 224, 114, 56),
            stop(100.0, 210, 64, 68),
        ],
    )
    .expect("built-in generic color table is valid")
}

fn stop(value: f32, r: u8, g: u8, b: u8) -> ColorStop {
    ColorStop {
        value,
        color: Rgba8::opaque(r, g, b),
    }
}

fn default_range_folded_color() -> Rgba8 {
    Rgba8::new(126, 80, 196, 245)
}

fn lerp_u8(left: u8, right: u8, amount: f32) -> u8 {
    ((left as f32 + (right as f32 - left as f32) * amount).round()).clamp(0.0, 255.0) as u8
}

fn normalize_line(line: &str) -> String {
    line.replace('\u{a0}', " ")
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    if let Some((key, value)) = line.split_once(':') {
        return Some((key, value));
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    Some((parts.next()?, parts.next()?))
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_color_stop(
    value: &str,
    expects_alpha: bool,
    line: usize,
) -> Result<ColorStop, ColorTableError> {
    let numbers = parse_numbers(value);
    let required = if expects_alpha { 5 } else { 4 };
    if numbers.len() < required {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "expected value plus RGB or RGBA components",
        });
    }
    let alpha = if expects_alpha {
        byte_component(numbers[4], line)?
    } else {
        255
    };
    Ok(ColorStop {
        value: numbers[0],
        color: Rgba8::new(
            byte_component(numbers[1], line)?,
            byte_component(numbers[2], line)?,
            byte_component(numbers[3], line)?,
            alpha,
        ),
    })
}

fn parse_color_only(value: &str, line: usize) -> Result<Rgba8, ColorTableError> {
    let numbers = parse_numbers(value);
    if numbers.len() < 3 {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "expected RGB components",
        });
    }
    Ok(Rgba8::new(
        byte_component(numbers[0], line)?,
        byte_component(numbers[1], line)?,
        byte_component(numbers[2], line)?,
        numbers
            .get(3)
            .map(|value| byte_component(*value, line))
            .transpose()?
            .unwrap_or(245),
    ))
}

fn parse_numbers(value: &str) -> Vec<f32> {
    value
        .split(|character: char| {
            character.is_ascii_whitespace() || character == ',' || character == ';'
        })
        .filter_map(|token| {
            let token = token.trim();
            (!token.is_empty())
                .then(|| token.parse::<f32>().ok())
                .flatten()
        })
        .collect()
}

fn byte_component(value: f32, line: usize) -> Result<u8, ColorTableError> {
    if !(0.0..=255.0).contains(&value) {
        return Err(ColorTableError::InvalidColor {
            line,
            reason: "color component must be 0-255",
        });
    }
    Ok(value.round() as u8)
}

fn parse_positive_f32(value: &str) -> Option<f32> {
    let value = value.trim().parse::<f32>().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

fn parse_sample_mode(value: &str) -> Option<SampleMode> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "false" | "no" | "off" | "0" | "step" | "stepped" | "discrete" | "nearest" => {
            Some(SampleMode::Stepped)
        }
        "true" | "yes" | "on" | "1" | "smooth" | "linear" | "interpolate" | "interpolated" => {
            Some(SampleMode::Interpolated)
        }
        _ => None,
    }
}

fn unit_value_to_mps_scale(units: &str) -> f32 {
    let units = units.trim().to_ascii_lowercase();
    match units.as_str() {
        "kt" | "kts" | "knot" | "knots" => KNOT_TO_MPS,
        "mph" | "mi/h" => MPH_TO_MPS,
        _ => 1.0,
    }
}

const RADARSCOPE_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -15 0 0 0 0
color: 5 29 37 60
color: 17.5 89 155 171
color: 22.5 33 186 72
color: 32.5 5 101 1
color: 37.5 251 252 0
color: 42.5 253 149 2
color: 50 253 38 0
color: 60 193 148 179
color: 70 165 2 215
color: 75 135 255 253
color: 80 173 99 64
color: 85 105 0 4
color: 95 0 0 0
"#;

const VORTEX_VELO_TABLE: &str = r#"
units: MPH
step: 20
scale: 2.237
product: BV
color: 0 115 115 115
color: .1 134 113 116
color: 5 130 3 3
color: 30 238 0 0
color: 40 255 87 1
color: 55 255 143 1
color: 70 255 239 2
color: 90 255 252 81
color: 120 255 255 255
color: 130 128 128 128
color: -4.99 70 129 68
color: -5 2 139 2
color: -30 4 239 16
color: -40 4 169 86
color: -55 4 92 162
color: -70 4 5 254
color: -90 4 87 254
color: -110 5 177 255
color: -130 0 255 255
"#;

const ANALYST_PRO_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
mode: stepped
color: -70 166 247 238
color: -58 95 222 202
color: -46 27 183 146
color: -36 0 140 92
color: -28 0 104 60
color: -21 0 170 70
color: -15 0 228 54
color: -10 16 164 45
color: -6 24 112 50
color: -2 64 96 72
color: 0 82 82 82
color: 2 105 64 64
color: 6 112 28 28
color: 10 166 22 22
color: 15 224 32 32
color: 21 255 64 64
color: 28 226 18 44
color: 36 176 14 48
color: 46 126 14 48
color: 58 222 176 186
color: 70 244 232 234
"#;

const NWS_VELOCITY_TABLE: &str = r#"
product: BV
units: kt
color: -120 0 255 255
color: -100 0 160 255
color: -80 0 64 255
color: -60 0 160 80
color: -40 0 220 0
color: -20 0 128 0
color: -5 85 145 85
color: 0 128 128 128
color: 5 150 90 90
color: 20 160 0 0
color: 40 230 0 0
color: 60 255 130 0
color: 80 255 230 0
color: 100 255 255 255
color: 120 170 170 170
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wxtools_velocity_units_and_unsorted_stops() {
        let table = ColorTable::parse(
            "Vortex Velo sample",
            r#"
            units: MPH
            product: BV
            color: 0 115 115 115
            color: 5 130 3 3
            color: -5 2 139 2
            "#,
        )
        .expect("table parses");

        assert_eq!(table.product(), Some("BV"));
        assert_eq!(table.stops()[0].value, -5.0 * MPH_TO_MPS);
        assert_eq!(table.sample(0.0), Rgba8::opaque(115, 115, 115));
    }

    #[test]
    fn parses_color4_and_range_folded_rows() {
        let table = ColorTable::parse(
            "RadarScope sample",
            r#"
            product: BR
            units: dBZ
            color4: -15 0 0 0 0
            color: 5 29 37 60
            RF: 82 21 86
            "#,
        )
        .expect("table parses");

        assert_eq!(table.sample(-20.0), Rgba8::TRANSPARENT);
        assert_eq!(table.range_folded_rgba(), Rgba8::new(82, 21, 86, 245));
    }

    #[test]
    fn parses_gr_scale_without_double_scaling_units() {
        let table = ColorTable::parse(
            "Scaled velocity",
            r#"
            product: BV
            scale: 2
            color: 10 10 20 30
            color: 20 30 40 50
            "#,
        )
        .expect("table parses");

        assert_eq!(table.stops()[0].value, 5.0);
        assert_eq!(table.stops()[1].value, 10.0);
    }

    #[test]
    fn stepped_tables_hold_bins_between_thresholds() {
        let table = ColorTable::parse(
            "Stepped velocity",
            r#"
            mode: stepped
            color: 0 0 0 0
            color: 10 255 255 255
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(0, 0, 0));
        assert_eq!(table.sample(10.0), Rgba8::opaque(255, 255, 255));
    }

    #[test]
    fn step_rows_make_pal_style_tables_stepped() {
        let table = ColorTable::parse(
            "RadarScope sample",
            r#"
            product: BR
            units: dBZ
            step: 5
            color4: -15 0 0 0 0
            color: 5 29 37 60
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample_mode_label(), "stepped");
        assert_eq!(table.sample(-10.0), Rgba8::TRANSPARENT);
        assert_eq!(table.sample(0.0), Rgba8::TRANSPARENT);
        assert_eq!(table.sample(5.0), Rgba8::opaque(29, 37, 60));
    }

    #[test]
    fn parse_stepped_defaults_to_bins_without_mode_line() {
        let table = ColorTable::parse_stepped(
            "NWS sample",
            r#"
            units: dBZ
            color: 0 0 0 0
            color: 10 255 255 255
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(0, 0, 0));
    }

    #[test]
    fn explicit_interpolated_mode_overrides_stepped_default() {
        let table = ColorTable::parse_stepped(
            "Smooth sample",
            r#"
            mode: interpolated
            color: 0 0 0 0
            color: 10 100 100 100
            "#,
        )
        .expect("table parses");

        assert!(table.interpolates());
        assert_eq!(table.sample(5.0), Rgba8::opaque(50, 50, 50));
    }

    #[test]
    fn radarscope_reflectivity_preset_is_stepped() {
        let table = builtin_reflectivity_table();

        assert_eq!(table.name(), "WxTools RadarScope BR");
        assert!(!table.interpolates());
        assert_eq!(table.sample_mode_label(), "stepped");
        assert_eq!(table.sample(0.0), Rgba8::TRANSPARENT);
        assert_eq!(table.sample(5.0), Rgba8::opaque(29, 37, 60));
    }

    #[test]
    fn builtin_radar_presets_default_to_stepped_sampling() {
        for table in [
            builtin_reflectivity_table(),
            analyst_reflectivity_table(),
            nws_reflectivity_table(),
            builtin_velocity_table(),
            vortex_velocity_table(),
            nws_velocity_table(),
        ] {
            assert!(
                !table.interpolates(),
                "{} should use stepped radar bins",
                table.name()
            );
        }
    }

    #[test]
    fn analyst_velocity_preset_is_stepped_for_gate_readability() {
        let table = analyst_velocity_table();

        assert!(!table.interpolates());
    }

    #[test]
    fn default_velocity_table_avoids_orange_yellow_purple_bins() {
        let table = builtin_velocity_table();

        assert_eq!(table.name(), "Analyst Pro VEL");
        assert!(!table.interpolates());
        for stop in table.stops() {
            let [red, green, blue, alpha] = stop.color.to_array();
            assert_eq!(alpha, 255);
            let orange_or_yellow = red > 210 && green > 100 && blue < 90;
            let purple = red > 90 && blue > 115 && green < 100;
            assert!(
                !orange_or_yellow && !purple,
                "bad velocity hue at {}: {red},{green},{blue}",
                stop.value
            );
        }
    }

    #[test]
    fn signatures_change_when_colors_change() {
        let left =
            ColorTable::parse("a", "color: 0 0 0 0\ncolor: 1 255 255 255").expect("table parses");
        let right =
            ColorTable::parse("a", "color: 0 0 0 0\ncolor: 1 255 255 254").expect("table parses");

        assert_ne!(left.signature(), right.signature());
    }

    #[test]
    fn built_in_presets_offer_multiple_ref_and_velocity_choices() {
        assert!(builtin_tables_for_family(ColorTableFamily::Reflectivity).len() >= 3);
        assert!(builtin_tables_for_family(ColorTableFamily::Velocity).len() >= 3);
    }
}
