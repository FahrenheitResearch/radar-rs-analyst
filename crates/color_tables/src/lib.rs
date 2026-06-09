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
                "step" => {
                    sample_mode = parse_positive_f32(value)
                        .map(|step| SampleMode::QuantizedInterpolated { step, origin: 0.0 })
                        .unwrap_or(SampleMode::Stepped);
                }
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
            sample_mode = sample_mode.scale_values(unit_scale);
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

    pub fn step_size(&self) -> Option<f32> {
        self.sample_mode.step_size()
    }

    pub fn sample(&self, value: f32) -> Rgba8 {
        if !value.is_finite() {
            return Rgba8::TRANSPARENT;
        }
        match self.sample_mode {
            SampleMode::Interpolated => self.sample_interpolated(value),
            SampleMode::Stepped => self.sample_stepped(value),
            SampleMode::QuantizedInterpolated { step, origin } => {
                if let Some(first_opaque_value) = self.first_opaque_value()
                    && value < first_opaque_value
                {
                    return Rgba8::TRANSPARENT;
                }
                let quantized = quantize_value(value, step, origin);
                self.sample_interpolated(quantized)
            }
        }
    }

    fn sample_interpolated(&self, value: f32) -> Rgba8 {
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
        let span = (right.value - left.value).max(f32::EPSILON);
        left.color.lerp(right.color, (value - left.value) / span)
    }

    fn sample_stepped(&self, value: f32) -> Rgba8 {
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
        self.stops[index - 1].color
    }

    fn first_opaque_value(&self) -> Option<f32> {
        let first = self.stops.first()?;
        (first.color.a == 0).then(|| {
            self.stops
                .iter()
                .find(|stop| stop.color.a > 0)
                .map(|stop| stop.value)
        })?
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

    pub fn mirrored_values(&self, name: impl Into<String>) -> Self {
        let stops = self
            .stops
            .iter()
            .map(|stop| ColorStop {
                value: -stop.value,
                color: stop.color,
            })
            .collect::<Vec<_>>();
        Self::from_parts(
            name.into(),
            self.product.clone(),
            self.units.clone(),
            self.range_folded,
            self.sample_mode.mirrored_values(),
            stops,
        )
        .expect("mirrored table preserves valid stops")
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleMode {
    Interpolated,
    Stepped,
    QuantizedInterpolated { step: f32, origin: f32 },
}

impl SampleMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interpolated => "interpolated",
            Self::Stepped => "stepped",
            Self::QuantizedInterpolated { .. } => "quantized stepped",
        }
    }

    fn step_size(self) -> Option<f32> {
        match self {
            Self::QuantizedInterpolated { step, .. } => Some(step),
            Self::Interpolated | Self::Stepped => None,
        }
    }

    fn scale_values(self, scale: f32) -> Self {
        match self {
            Self::QuantizedInterpolated { step, origin } => Self::QuantizedInterpolated {
                step: step * scale,
                origin: origin * scale,
            },
            Self::Interpolated | Self::Stepped => self,
        }
    }

    fn mirrored_values(self) -> Self {
        match self {
            Self::QuantizedInterpolated { step, origin } => Self::QuantizedInterpolated {
                step,
                origin: -origin,
            },
            Self::Interpolated | Self::Stepped => self,
        }
    }
}

impl Hash for SampleMode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match *self {
            Self::Interpolated => 0_u8.hash(state),
            Self::Stepped => 1_u8.hash(state),
            Self::QuantizedInterpolated { step, origin } => {
                2_u8.hash(state);
                step.to_bits().hash(state);
                origin.to_bits().hash(state);
            }
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
    gr2_reflectivity_table()
}

pub fn builtin_velocity_table() -> ColorTable {
    tornado_velocity_table()
}

pub fn tornado_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Tornado VEL", TORNADO_VELOCITY_TABLE)
        .expect("built-in tornado velocity color table is valid")
}

pub fn vortex_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("WxTools Vortex Velo", VORTEX_VELO_TABLE)
        .expect("built-in velocity color table is valid")
}

pub fn builtin_tables_for_family(family: ColorTableFamily) -> Vec<ColorTable> {
    match family {
        ColorTableFamily::Reflectivity => vec![
            builtin_reflectivity_table(),
            analyst_classic_reflectivity_table(),
            nws_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
            tornado_debris_reflectivity_table(),
            clean_light_reflectivity_table(),
        ],
        ColorTableFamily::Velocity => vec![
            builtin_velocity_table(),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
            sign_check_velocity_table(),
            couplet_pop_velocity_table(),
            gr2_ish_analyst_velocity_table(),
            subtle_srv_velocity_table(),
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
    ColorTable::parse_stepped("NWS Classic REF", NWS_CLASSIC_REFLECTIVITY_TABLE)
        .expect("built-in nws reflectivity color table is valid")
}

pub fn analyst_classic_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Classic REF", ANALYST_CLASSIC_REFLECTIVITY_TABLE)
        .expect("built-in analyst classic reflectivity color table is valid")
}

pub fn gr2_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("GR2Analyst Classic REF", GR2_REFLECTIVITY_TABLE)
        .expect("built-in GR2 reflectivity color table is valid")
}

pub fn storm_detail_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Storm Detail REF", STORM_DETAIL_REFLECTIVITY_TABLE)
        .expect("built-in storm detail reflectivity color table is valid")
}

pub fn hail_core_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Hail Core REF", HAIL_CORE_REFLECTIVITY_TABLE)
        .expect("built-in hail core reflectivity color table is valid")
}

pub fn low_precip_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Low Precip REF", LOW_PRECIP_REFLECTIVITY_TABLE)
        .expect("built-in low precip reflectivity color table is valid")
}

pub fn dark_scope_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Dark Scope REF", DARK_SCOPE_REFLECTIVITY_TABLE)
        .expect("built-in dark scope reflectivity color table is valid")
}

pub fn tornado_debris_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Tornado Debris REF", TORNADO_DEBRIS_REFLECTIVITY_TABLE)
        .expect("built-in tornado debris reflectivity color table is valid")
}

pub fn clean_light_reflectivity_table() -> ColorTable {
    ColorTable::parse_stepped("Clean Light REF", CLEAN_LIGHT_REFLECTIVITY_TABLE)
        .expect("built-in clean light reflectivity color table is valid")
}

pub fn analyst_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Pro VEL", ANALYST_PRO_VELOCITY_TABLE)
        .expect("built-in analyst velocity color table is valid")
}

pub fn nws_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("NWS Classic VEL", NWS_VELOCITY_TABLE)
        .expect("built-in nws velocity color table is valid")
}

pub fn gr2_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("GR2Analyst Classic VEL", GR2_VELOCITY_TABLE)
        .expect("built-in GR2 velocity color table is valid")
}

pub fn tight_couplet_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Analyst Tight Couplet VEL", TIGHT_COUPLET_VELOCITY_TABLE)
        .expect("built-in tight couplet velocity color table is valid")
}

pub fn radarscope_contrast_velocity_table() -> ColorTable {
    ColorTable::parse_stepped(
        "RadarScope Contrast VEL",
        RADARSCOPE_CONTRAST_VELOCITY_TABLE,
    )
    .expect("built-in radarscope contrast velocity color table is valid")
}

pub fn sign_check_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Sign Check VEL", SIGN_CHECK_VELOCITY_TABLE)
        .expect("built-in sign-check velocity color table is valid")
}

pub fn couplet_pop_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Couplet Pop VEL", COUPLET_POP_VELOCITY_TABLE)
        .expect("built-in couplet pop velocity color table is valid")
}

pub fn gr2_ish_analyst_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("GR2-ish Analyst VEL", GR2_ISH_ANALYST_VELOCITY_TABLE)
        .expect("built-in GR2-ish analyst velocity color table is valid")
}

pub fn subtle_srv_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Subtle SRV VEL", SUBTLE_SRV_VELOCITY_TABLE)
        .expect("built-in subtle SRV velocity color table is valid")
}

pub fn nws_split_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("NWS Split VEL", NWS_SPLIT_VELOCITY_TABLE)
        .expect("built-in split velocity color table is valid")
}

pub fn dark_analyst_velocity_table() -> ColorTable {
    ColorTable::parse_stepped("Dark Analyst VEL", DARK_ANALYST_VELOCITY_TABLE)
        .expect("built-in dark analyst velocity color table is valid")
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

fn quantize_value(value: f32, step: f32, origin: f32) -> f32 {
    if !step.is_finite() || step <= 0.0 {
        return value;
    }
    ((value - origin) / step).round() * step + origin
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
    let value = parse_numbers(value).first().copied()?;
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

const GR2_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 4 233 231
color: 15 1 159 244
color: 20 3 0 244
color: 25 2 253 2
color: 30 1 197 1
color: 35 0 142 0
color: 40 253 248 2
color: 45 229 188 0
color: 50 253 149 0
color: 55 253 0 0
color: 62.5 212 0 0
color: 67.5 188 0 0
color: 72.5 232 32 206
color: 80 156 70 206
color: 92.5 255 255 255
"#;

const NWS_CLASSIC_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 4 233 231
color: 15 1 159 244
color: 20 3 0 244
color: 25 2 253 2
color: 30 1 197 1
color: 35 0 142 0
color: 40 253 248 2
color: 45 229 188 0
color: 50 253 149 0
color: 55 253 0 0
color: 62.5 212 0 0
color: 67.5 188 0 0
color: 72.5 232 32 206
color: 80 156 70 206
color: 92.5 255 255 255
"#;

const ANALYST_CLASSIC_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 0 204 220
color: 15 0 132 232
color: 20 12 58 226
color: 25 0 222 44
color: 30 0 174 24
color: 35 0 124 12
color: 40 235 226 34
color: 45 238 174 28
color: 50 242 112 22
color: 55 238 28 30
color: 62.5 190 0 18
color: 67.5 150 0 18
color: 72.5 214 42 180
color: 80 150 82 198
color: 92.5 246 246 246
"#;

const STORM_DETAIL_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 2.5
color4: -10 0 0 0 0
color4: 0 0 0 0 0
color: 5 18 42 86
color: 10 25 92 154
color: 15 31 164 206
color: 20 28 184 114
color: 25 21 132 44
color: 30 88 178 42
color: 35 218 226 45
color: 40 251 180 32
color: 45 254 101 22
color: 50 238 32 28
color: 55 174 0 22
color: 60 214 52 168
color: 65 142 34 214
color: 70 228 228 236
color: 80 255 255 255
"#;

const HAIL_CORE_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 35 98 164
color: 15 33 168 210
color: 20 16 172 78
color: 25 0 120 36
color: 30 82 170 40
color: 35 234 232 36
color: 40 252 168 22
color: 45 252 88 18
color: 50 246 26 28
color: 57.5 176 0 16
color: 65 154 0 28
color: 70 206 32 174
color: 77.5 152 74 204
color: 80 255 255 255
color: 87.5 112 228 255
color: 95 255 255 255
"#;

const LOW_PRECIP_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 2.5
color4: -15 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 38 116 174
color: 15 42 184 214
color: 20 58 204 132
color: 25 44 154 66
color: 30 84 188 50
color: 35 224 226 64
color: 40 250 178 50
color: 45 244 96 42
color: 50 218 44 52
color: 57.5 160 26 78
color: 65 170 28 128
color: 72.5 202 68 196
color: 80 154 84 204
color: 90 238 238 244
"#;

const DARK_SCOPE_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 38 86 128
color: 15 52 136 170
color: 20 30 158 86
color: 25 18 118 48
color: 30 78 164 44
color: 35 196 206 54
color: 40 232 156 42
color: 45 234 88 34
color: 50 218 38 40
color: 57.5 156 24 30
color: 65 168 30 130
color: 72.5 196 70 204
color: 80 154 82 210
color: 87.5 226 226 232
color: 95 255 255 255
"#;

const TORNADO_DEBRIS_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 30 96 152
color: 15 34 152 196
color: 20 26 190 112
color: 25 0 146 52
color: 30 72 176 42
color: 35 214 220 48
color: 40 246 174 32
color: 45 250 102 26
color: 50 238 32 30
color: 57.5 178 0 24
color: 65 164 0 40
color: 70 206 36 168
color: 77.5 224 94 210
color: 87.5 176 230 255
color: 95 255 255 255
"#;

const CLEAN_LIGHT_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 2.5
color4: -15 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 30 114 160
color: 17.5 38 164 190
color: 22.5 42 186 110
color: 27.5 22 132 52
color: 32.5 94 176 48
color: 37.5 220 218 58
color: 42.5 242 160 42
color: 47.5 236 90 38
color: 52.5 218 38 44
color: 60 156 22 34
color: 67.5 174 34 132
color: 75 206 72 198
color: 82.5 156 84 206
color: 92.5 238 238 242
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

const TORNADO_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 236 255 255
color: -58 126 220 255
color: -48 166 236 255
color: -38 210 250 255
color: -30 246 255 255
color: -24 232 255 250
color: -18 0 156 54
color: -13 18 232 54
color: -9 82 244 104
color: -5 36 136 54
color: -2 84 100 84
color: 0 112 112 112
color: 2 120 86 84
color: 5 154 46 44
color: 9 216 28 28
color: 14 255 34 40
color: 20 242 0 0
color: 24 255 238 218
color: 28 255 255 238
color: 34 255 224 168
color: 42 255 248 220
color: 50 255 255 240
color: 58 255 230 190
color: 64 255 202 130
color: 70 255 240 204
"#;

const GR2_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 255 255
color: -55 0 170 255
color: -42 0 80 255
color: -32 0 180 80
color: -24 0 220 0
color: -16 0 148 0
color: -8 74 132 74
color: -2 96 108 96
color: 0 128 128 128
color: 2 126 94 94
color: 8 156 44 44
color: 16 198 0 0
color: 24 244 0 0
color: 32 255 116 0
color: 42 255 220 0
color: 55 255 255 255
color: 70 172 172 172
"#;

const TIGHT_COUPLET_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 230 255 255
color: -50 54 236 214
color: -36 0 188 122
color: -26 0 114 48
color: -18 0 176 34
color: -12 32 252 46
color: -7 0 176 34
color: -3 36 112 50
color: -1 78 94 78
color: 0 112 112 112
color: 1 112 78 78
color: 3 152 36 36
color: 7 246 22 22
color: 12 255 42 42
color: 18 202 0 0
color: 26 142 0 0
color: 36 110 0 0
color: 50 238 124 132
color: 70 255 255 255
"#;

const RADARSCOPE_CONTRAST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 216 255 255
color: -58 126 220 255
color: -48 166 236 255
color: -38 210 250 255
color: -30 246 255 255
color: -24 232 255 250
color: -22 210 248 226
color: -16 0 224 54
color: -11 42 255 66
color: -7 106 240 116
color: -4 46 134 54
color: -1 98 104 96
color: 0 122 122 122
color: 1 128 96 96
color: 4 156 64 62
color: 7 198 42 42
color: 11 246 28 28
color: 16 255 40 46
color: 22 244 0 24
color: 24 255 238 218
color: 28 255 255 238
color: 36 255 220 172
color: 44 255 250 224
color: 50 255 255 238
color: 56 255 232 190
color: 62 255 204 134
color: 70 255 242 202
"#;

const SIGN_CHECK_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
mode: stepped
rf: 180 80 255 255
color: -100 0 0 255
color: -0.01 0 0 255
color: 0 120 120 120
color: 0.01 255 0 0
color: 100 255 0 0
"#;

const COUPLET_POP_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 238 255 255
color: -58 92 238 216
color: -46 20 206 152
color: -36 0 150 82
color: -28 0 92 42
color: -21 0 172 58
color: -15 0 236 44
color: -10 34 186 48
color: -6 36 122 50
color: -2 78 98 76
color: 0 92 92 92
color: 2 104 72 70
color: 6 132 34 34
color: 10 214 24 24
color: 15 255 34 34
color: 21 236 16 38
color: 28 180 8 34
color: 36 122 6 34
color: 46 196 78 96
color: 58 240 184 190
color: 70 255 255 255
"#;

const GR2_ISH_ANALYST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 252 252
color: -55 0 174 244
color: -42 20 90 238
color: -32 0 176 82
color: -24 0 214 0
color: -16 0 150 0
color: -8 74 132 74
color: -2 96 108 96
color: 0 124 124 124
color: 2 126 94 94
color: 8 160 42 42
color: 16 204 0 0
color: 24 246 0 0
color: 32 255 92 38
color: 42 246 156 128
color: 55 255 222 222
color: 70 172 172 172
"#;

const SUBTLE_SRV_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 184 236 230
color: -55 90 206 190
color: -42 32 168 132
color: -32 12 122 76
color: -24 18 88 52
color: -16 36 140 64
color: -10 62 196 82
color: -5 58 132 70
color: -1 82 98 84
color: 0 94 94 94
color: 1 104 86 84
color: 5 128 58 54
color: 10 188 52 48
color: 16 222 64 58
color: 24 184 42 54
color: 32 138 34 54
color: 42 190 96 114
color: 55 224 184 190
color: 70 242 242 242
"#;

const NWS_SPLIT_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 240 240
color: -55 0 150 240
color: -42 0 62 220
color: -32 0 150 60
color: -24 0 210 0
color: -16 0 136 0
color: -8 76 140 76
color: -2 104 118 104
color: 0 130 130 130
color: 2 142 104 104
color: 8 168 54 54
color: 16 210 0 0
color: 24 248 0 0
color: 32 255 118 0
color: 42 255 226 0
color: 55 255 255 255
color: 70 170 170 170
"#;

const DARK_ANALYST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 210 246 240
color: -55 82 210 196
color: -42 0 164 126
color: -32 0 114 68
color: -24 0 80 44
color: -16 0 142 50
color: -10 20 206 42
color: -5 34 126 46
color: -1 72 88 74
color: 0 94 94 94
color: 1 102 72 72
color: 5 132 34 34
color: 10 208 24 24
color: 16 238 42 42
color: 24 188 18 36
color: 32 128 16 36
color: 42 198 92 112
color: 55 232 202 206
color: 70 250 250 250
"#;

const ANALYST_PRO_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
mode: stepped
color: -70 222 255 255
color: -58 126 220 255
color: -46 170 238 255
color: -36 214 250 255
color: -28 246 255 255
color: -24 232 255 250
color: -21 210 248 226
color: -15 0 226 58
color: -10 42 214 70
color: -6 42 132 54
color: -2 82 98 80
color: 0 110 110 110
color: 2 116 84 84
color: 6 148 42 42
color: 10 204 30 30
color: 15 248 36 42
color: 21 255 78 86
color: 24 255 238 218
color: 28 255 255 238
color: 36 255 222 174
color: 46 255 250 226
color: 58 255 255 238
color: 66 255 210 146
color: 70 255 240 220
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
    fn step_rows_make_pal_style_tables_quantized_ramps() {
        let table = ColorTable::parse(
            "RadarScope sample",
            r#"
            product: BR
            units: dBZ
            step: 5
            color4: -5 0 0 0 0
            color: 5 0 0 100
            color: 15 0 0 200
            "#,
        )
        .expect("table parses");

        assert!(!table.interpolates());
        assert_eq!(table.sample_mode_label(), "quantized stepped");
        assert_eq!(table.step_size(), Some(5.0));
        assert_eq!(table.sample(0.0), Rgba8::TRANSPARENT);
        assert_eq!(table.sample(7.4), Rgba8::opaque(0, 0, 100));
        assert_eq!(table.sample(11.0), Rgba8::opaque(0, 0, 150));
        assert_eq!(table.sample(12.4), Rgba8::opaque(0, 0, 150));
        assert_eq!(table.sample(12.6), Rgba8::opaque(0, 0, 200));
    }

    #[test]
    fn quantized_step_converts_with_velocity_units() {
        let table = ColorTable::parse(
            "Velocity sample",
            r#"
            units: MPH
            step: 10
            color: 0 80 80 80
            color: 20 240 0 0
            "#,
        )
        .expect("table parses");

        let step = table.step_size().expect("numeric step preserved");
        assert!((step - 10.0 * MPH_TO_MPS).abs() < 0.001);
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
    fn default_reflectivity_preset_filters_low_dbz_and_stretches_high_end() {
        let table = builtin_reflectivity_table();

        assert_eq!(table.name(), "GR2Analyst Classic REF");
        assert!(!table.interpolates());
        assert_eq!(table.sample_mode_label(), "quantized stepped");
        assert_eq!(table.step_size(), Some(5.0));
        assert_eq!(table.sample(5.0), Rgba8::TRANSPARENT);
        assert_ne!(table.sample(10.0), Rgba8::TRANSPARENT);
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
    fn default_velocity_table_has_radarscope_style_velocity_contrast() {
        let table = builtin_velocity_table();

        assert_eq!(table.name(), "Analyst Tornado VEL");
        assert!(!table.interpolates());
        let zero = table.sample(0.0);
        let inbound = table.sample(-58.0);
        let inbound_core = table.sample(-9.0);
        let outbound = table.sample(14.0);
        let outbound_high = table.sample(50.0);
        let outbound_extreme = table.sample(64.0);
        let [zero_r, zero_g, zero_b, zero_a] = zero.to_array();
        assert_eq!(zero_a, 255);
        assert!((zero_r as i16 - zero_g as i16).abs() <= 8);
        assert!((zero_g as i16 - zero_b as i16).abs() <= 8);

        let [in_r, in_g, in_b, _] = inbound.to_array();
        assert!(in_b > 240 && in_g > 180 && in_r < 180);
        let [core_r, core_g, core_b, _] = inbound_core.to_array();
        assert!(core_g > 220 && core_r < 120 && core_b < 140);

        let [out_r, out_g, out_b, _] = outbound.to_array();
        assert!(out_r > 230 && out_g < 90 && out_b < 90);
        let [high_r, high_g, high_b, _] = outbound_high.to_array();
        assert!(high_r > 230 && high_g > 120 && high_b > 160);
        let [extreme_r, extreme_g, extreme_b, _] = outbound_extreme.to_array();
        assert!(extreme_r > 230 && extreme_g > 170 && extreme_b > 110);
    }

    #[test]
    fn accepted_velocity_presets_whiten_strong_wind_cores() {
        for table in [
            builtin_velocity_table(),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
        ] {
            let inbound = table.sample(-30.0);
            let [in_r, in_g, in_b, _] = inbound.to_array();
            assert!(
                in_r > 185 && in_g > 235 && in_b > 220,
                "{} should turn strong inbound winds pale cyan/white, got {in_r},{in_g},{in_b}",
                table.name()
            );

            let outbound = table.sample(36.0);
            let [out_r, out_g, out_b, _] = outbound.to_array();
            assert!(
                out_r > 240 && out_g > 190 && out_b > 140,
                "{} should turn strong outbound winds cream/orange-white, got {out_r},{out_g},{out_b}",
                table.name()
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
        let reflectivity = builtin_tables_for_family(ColorTableFamily::Reflectivity)
            .into_iter()
            .map(|table| table.name().to_owned())
            .collect::<Vec<_>>();
        let velocity = builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .map(|table| table.name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            reflectivity,
            vec![
                "GR2Analyst Classic REF",
                "Analyst Classic REF",
                "NWS Classic REF",
                "Dark Scope REF",
                "Analyst Hail Core REF",
                "Analyst Low Precip REF",
                "Tornado Debris REF",
                "Clean Light REF",
            ]
        );
        assert_eq!(
            velocity,
            vec![
                "Analyst Tornado VEL",
                "Analyst Pro VEL",
                "RadarScope Contrast VEL",
                "Sign Check VEL",
                "Couplet Pop VEL",
                "GR2-ish Analyst VEL",
                "Subtle SRV VEL",
            ]
        );
    }

    #[test]
    fn accepted_reflectivity_presets_filter_junk_and_delay_purple() {
        for table in [
            gr2_reflectivity_table(),
            nws_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
        ] {
            assert_eq!(table.sample_mode_label(), "quantized stepped");
            assert!(
                table.step_size().is_some(),
                "{} has step size",
                table.name()
            );
            assert_eq!(table.sample(5.0), Rgba8::TRANSPARENT);
            assert_ne!(
                table.sample(10.0),
                Rgba8::TRANSPARENT,
                "{} should show 10 dBZ and higher",
                table.name()
            );
            for stop in table.stops() {
                let [red, green, blue, alpha] = stop.color.to_array();
                let purple_or_magenta = alpha > 0 && red > 120 && blue > 120 && green < 120;
                assert!(
                    !purple_or_magenta || stop.value >= 65.0,
                    "{} brings purple too early at {:.1} dBZ: {red},{green},{blue}",
                    table.name(),
                    stop.value
                );
            }
        }
    }

    #[test]
    fn accepted_reflectivity_presets_keep_high_dbz_purple() {
        for table in [
            gr2_reflectivity_table(),
            nws_reflectivity_table(),
            analyst_classic_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
        ] {
            assert!(
                table.stops().iter().any(|stop| {
                    let [red, green, blue, alpha] = stop.color.to_array();
                    alpha > 0 && stop.value >= 65.0 && red > 140 && blue > 120 && green < 120
                }),
                "{} should keep a high-dBZ purple/magenta bin",
                table.name()
            );
        }
    }

    #[test]
    fn accepted_velocity_presets_stay_available() {
        for table in [
            builtin_velocity_table(),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
            sign_check_velocity_table(),
        ] {
            assert!(!table.interpolates());
        }
    }

    #[test]
    fn sign_check_velocity_table_exposes_raw_velocity_polarity() {
        let table = sign_check_velocity_table();

        assert_eq!(table.name(), "Sign Check VEL");
        assert_eq!(table.sample_mode_label(), "stepped");
        assert_eq!(table.sample(-1.0), Rgba8::opaque(0, 0, 255));
        assert_eq!(table.sample(0.0), Rgba8::opaque(120, 120, 120));
        assert_eq!(table.sample(1.0), Rgba8::opaque(255, 0, 0));
        assert_eq!(table.range_folded_rgba(), Rgba8::opaque(180, 80, 255));
    }

    #[test]
    fn mirrored_velocity_table_samples_opposite_polarity_colors() {
        let table = sign_check_velocity_table();
        let mirrored = table.mirrored_values("Mirrored Sign Check VEL");

        assert_eq!(mirrored.sample(1.0), table.sample(-1.0));
        assert_eq!(mirrored.sample(-1.0), table.sample(1.0));
        assert_eq!(mirrored.sample(0.0), table.sample(0.0));
        assert_eq!(mirrored.range_folded_rgba(), table.range_folded_rgba());
    }

    #[test]
    fn review_candidate_palettes_are_stepped() {
        for table in [
            analyst_classic_reflectivity_table(),
            tornado_debris_reflectivity_table(),
            clean_light_reflectivity_table(),
            couplet_pop_velocity_table(),
            gr2_ish_analyst_velocity_table(),
            subtle_srv_velocity_table(),
        ] {
            assert!(!table.interpolates(), "{} should be stepped", table.name());
        }
    }
}
