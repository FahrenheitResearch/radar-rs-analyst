//! Fast color table parsing and sampling for radar renderers.

pub mod hazards;

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

/// Which physical domain a colour table is drawn over.
///
/// The dual-pol moments each get their own family because their domains have
/// nothing in common: ZDR is a small signed decibel ratio, CC is a bounded
/// correlation crowded against 1.0, PHI is a 360-degree angle that wraps, and
/// KDP is a small signed gradient. A single "other" ramp over 0..100 - which is
/// what all four shared before - leaves every one of them a flat wash, because
/// the whole observed distribution of each falls inside a single stop interval.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColorTableFamily {
    Reflectivity,
    Velocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
    Generic,
}

impl ColorTableFamily {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reflectivity => "Reflectivity",
            Self::Velocity => "Velocity / SRV",
            Self::SpectrumWidth => "Spectrum Width",
            Self::DifferentialReflectivity => "Differential Reflectivity (ZDR)",
            Self::CorrelationCoefficient => "Correlation Coefficient (CC)",
            Self::DifferentialPhase => "Differential Phase (PHI)",
            Self::SpecificDifferentialPhase => "Specific Differential Phase (KDP)",
            Self::Generic => "Other",
        }
    }

    /// Every family, in the order a picker should list them.
    ///
    /// Base moments first because they are what an analyst reaches for most,
    /// then dual-pol in the order the moments appear in a NEXRAD message, then
    /// the catch-all last.
    pub const ALL: [Self; 8] = [
        Self::Reflectivity,
        Self::Velocity,
        Self::SpectrumWidth,
        Self::DifferentialReflectivity,
        Self::CorrelationCoefficient,
        Self::DifferentialPhase,
        Self::SpecificDifferentialPhase,
        Self::Generic,
    ];

    /// The engine-value domain the family's moment lives on.
    ///
    /// A caller that has to size a histogram axis or synthesise a ramp needs a
    /// range for the *moment* without having to sniff whichever table happens
    /// to be selected. Ranges follow Ryzhkov and Zrnic (2019), *Radar
    /// Polarimetry for Weather Observations*, Springer,
    /// doi:10.1007/978-3-030-05093-1, taken where possible from the WSR-88D
    /// Level II field encodings themselves - see `ZDR_MIN_DB` and `CC_MIN`.
    ///
    /// It is **not** a legend range. For the five dual-pol and spectrum-width
    /// families every built-in table is drawn edge to edge on this domain, and
    /// a test pins that. The three older families are not: the reflectivity
    /// presets ink from -15 or -10 dBZ to between 75 and 95, the velocity
    /// presets from -70 to +70 except Sign Check VEL at +/-100, and Generic is a
    /// placeholder ramp. A legend must therefore use the selected table's
    /// `inked_value_span`, which is what it paints, and keep this for the axis
    /// it is laid out against.
    pub fn nominal_domain(self) -> (f32, f32) {
        match self {
            Self::Reflectivity => (-32.0, 95.0),
            Self::Velocity => (-70.0, 70.0),
            Self::SpectrumWidth => (0.0, 24.0),
            Self::DifferentialReflectivity => (ZDR_MIN_DB, ZDR_MAX_DB),
            Self::CorrelationCoefficient => (CC_MIN, CC_MAX),
            Self::DifferentialPhase => (PHI_MIN_DEG, PHI_MAX_DEG),
            Self::SpecificDifferentialPhase => (KDP_MIN_DEG_PER_KM, KDP_MAX_DEG_PER_KM),
            Self::Generic => (0.0, 100.0),
        }
    }

    /// Whether the family's domain wraps, so the first and last colour must
    /// agree. Only differential phase does.
    pub fn is_cyclic(self) -> bool {
        matches!(self, Self::DifferentialPhase)
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

    /// The engine-value range over which this table actually puts ink on the
    /// screen: from the first stop with a non-zero alpha to the last such stop.
    ///
    /// Prevents a legend that advertises a range where nothing is ever painted.
    /// Twelve of the forty-seven built-in tables open with exactly two alpha-0
    /// stops: every one of the twelve reflectivity presets that has any
    /// transparency at all - the nine parsed ones and the three interpolated
    /// ones. They declare their first stop at -10 dBZ (Low Precip and Clean
    /// Light at -15), so a legend bar drawn across the declared domain labels
    /// ticks from -10 or -15 dBZ over a stretch of scope that stays empty no
    /// matter what the radar returns. Eleven of the twelve ink from 10 dBZ;
    /// Storm Detail, whose second transparent stop sits at 0 rather than 7.5,
    /// inks from 5 dBZ. The other thirty-five built-ins have no transparent
    /// stop at all and report their full declared range.
    ///
    /// "Inked" here means "where the palette varies", not "where pixels appear".
    /// `sample()` clamps outside the stop range in every mode instead of fading
    /// out, so a value past the last stop is still drawn in the last stop's
    /// color. The span is the interval a legend should label, never a claim
    /// about which pixels get covered.
    ///
    /// The range-folded color is deliberately excluded even though it is opaque:
    /// it is selected by the folded code in the moment data, which render2d
    /// intercepts before value conversion, so it has no engine value and letting
    /// it widen the span would hang a number on a non-numeric category.
    ///
    /// Returns `None` only when every stop is fully transparent; such a table
    /// can never ink anything and its legend must be suppressed, not drawn
    /// empty. Note this differs from `first_opaque_value`, which reports `None`
    /// for the common case of a palette whose first stop is already opaque.
    ///
    /// The two bounds can be equal. `from_parts` guarantees at least two stops
    /// but not two *inked* stops, so a loaded palette of one transparent stop
    /// and one opaque stop reports a zero-width span such as `(10.0, 10.0)`.
    /// That is the honest answer - the table inks exactly one value - but a
    /// legend that places a tick at `(value - low) / (high - low)` divides by
    /// zero and gets NaN coordinates, so the caller must test for `high == low`
    /// before laying out a bar. No built-in table has this shape.
    pub fn inked_value_span(&self) -> Option<(f32, f32)> {
        let mut first_inked: Option<f32> = None;
        let mut last_inked: Option<f32> = None;
        for stop in &self.stops {
            if stop.color.a == 0 {
                continue;
            }
            if first_inked.is_none() {
                first_inked = Some(stop.value);
            }
            last_inked = Some(stop.value);
        }
        match (first_inked, last_inked) {
            (Some(low), Some(high)) => Some((low, high)),
            _ => None,
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
    differential_reflectivity: ColorTable,
    correlation_coefficient: ColorTable,
    differential_phase: ColorTable,
    specific_differential_phase: ColorTable,
    generic: ColorTable,
}

impl ColorTableSet {
    pub fn for_family(&self, family: ColorTableFamily) -> &ColorTable {
        match family {
            ColorTableFamily::Reflectivity => &self.reflectivity,
            ColorTableFamily::Velocity => &self.velocity,
            ColorTableFamily::SpectrumWidth => &self.spectrum_width,
            ColorTableFamily::DifferentialReflectivity => &self.differential_reflectivity,
            ColorTableFamily::CorrelationCoefficient => &self.correlation_coefficient,
            ColorTableFamily::DifferentialPhase => &self.differential_phase,
            ColorTableFamily::SpecificDifferentialPhase => &self.specific_differential_phase,
            ColorTableFamily::Generic => &self.generic,
        }
    }

    pub fn set_family(&mut self, family: ColorTableFamily, table: ColorTable) {
        match family {
            ColorTableFamily::Reflectivity => self.reflectivity = table,
            ColorTableFamily::Velocity => self.velocity = table,
            ColorTableFamily::SpectrumWidth => self.spectrum_width = table,
            ColorTableFamily::DifferentialReflectivity => self.differential_reflectivity = table,
            ColorTableFamily::CorrelationCoefficient => self.correlation_coefficient = table,
            ColorTableFamily::DifferentialPhase => self.differential_phase = table,
            ColorTableFamily::SpecificDifferentialPhase => self.specific_differential_phase = table,
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
            differential_reflectivity: builtin_differential_reflectivity_table(),
            correlation_coefficient: builtin_correlation_coefficient_table(),
            differential_phase: builtin_differential_phase_table(),
            specific_differential_phase: builtin_specific_differential_phase_table(),
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

// ---------------------------------------------------------------------------
// Built-in palettes
//
// Every constructor below is built through one of the three helpers here, so
// two things hold for every built-in without anyone having to remember them.
//
// It is validated at construction and panics carrying its own name if it is
// not, rather than silently painting nothing at the first gate that needs it.
//
// And its name ends with its own sampling mode. That matters because the name
// is the only string a picker row shows. A stepped table paints flat bands and
// an interpolated one paints a gradient; those are wildly different pictures of
// the same field, and a list reading "GR2Analyst Classic REF" beside "Smooth
// Classic REF" gives an analyst no way to tell which is which. Reading
// "GR2Analyst Classic REF (quantized stepped)" beside "Smooth Classic REF
// (interpolated)" answers it with no hover and no click.
// ---------------------------------------------------------------------------

/// Stamp a table's own sampling mode onto the end of its name.
///
/// The wording is `SampleMode::label`'s, the same string `sample_mode_label`
/// reports, so a name and a tooltip can never disagree about one table.
///
/// Read off the built table rather than off whatever mode was asked for:
/// `parse_stepped` only sets a *default*, and a palette carrying `step:` or
/// `mode:` overrides it. Most of the parsed built-ins do exactly that.
fn named_by_sample_mode(table: ColorTable) -> ColorTable {
    let name = format!("{} ({})", table.name, table.sample_mode.label());
    ColorTable { name, ..table }
}

/// A palette written in the GR/RadarScope text format, sampled as its own
/// header asks.
fn parsed_preset(name: &str, text: &str) -> ColorTable {
    named_by_sample_mode(
        ColorTable::parse_stepped(name, text)
            .unwrap_or_else(|error| panic!("built-in palette {name} is invalid: {error}")),
    )
}

/// A palette given as stops, sampled by interpolating between them.
fn smooth_preset(name: &str, stops: Vec<ColorStop>) -> ColorTable {
    named_by_sample_mode(
        ColorTable::new(name, stops)
            .unwrap_or_else(|error| panic!("built-in palette {name} is invalid: {error}")),
    )
}

/// A palette given as stops, sampled as one flat band per stop interval.
fn banded_preset(name: &str, stops: Vec<ColorStop>) -> ColorTable {
    named_by_sample_mode(
        ColorTable::new_stepped(name, stops)
            .unwrap_or_else(|error| panic!("built-in palette {name} is invalid: {error}")),
    )
}

pub fn builtin_reflectivity_table() -> ColorTable {
    gr2_reflectivity_table()
}

pub fn builtin_velocity_table() -> ColorTable {
    tornado_velocity_table()
}

pub fn tornado_velocity_table() -> ColorTable {
    parsed_preset("Analyst Tornado VEL", TORNADO_VELOCITY_TABLE)
}

pub fn vortex_velocity_table() -> ColorTable {
    parsed_preset("WxTools Vortex Velo", VORTEX_VELO_TABLE)
}

/// Every table a picker should offer for one family, defaults first.
///
/// The head of each list is that family's default, which
/// `every_family_default_is_the_first_table_the_picker_offers` pins, and the
/// three interpolated reflectivity presets and two interpolated velocity
/// presets follow immediately behind their stepped defaults. That ordering is
/// the point: the alternative sitting next to the default is the same field
/// drawn the other way, so the first click an analyst makes is the one that
/// tells them whether a banded scope is the palette or the renderer.
pub fn builtin_tables_for_family(family: ColorTableFamily) -> Vec<ColorTable> {
    match family {
        ColorTableFamily::Reflectivity => vec![
            builtin_reflectivity_table(),
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
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
            smooth_doppler_velocity_table(),
            smooth_couplet_velocity_table(),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
            sign_check_velocity_table(),
            couplet_pop_velocity_table(),
            gr2_ish_analyst_velocity_table(),
            subtle_srv_velocity_table(),
        ],
        ColorTableFamily::SpectrumWidth => vec![
            builtin_spectrum_width_table(),
            turbulence_spectrum_width_table(),
            clear_air_spectrum_width_table(),
            spectrum_width_class_bands_table(),
        ],
        ColorTableFamily::DifferentialReflectivity => vec![
            builtin_differential_reflectivity_table(),
            storm_interrogation_differential_reflectivity_table(),
            zdr_column_hunter_table(),
            hail_signal_differential_reflectivity_table(),
        ],
        ColorTableFamily::CorrelationCoefficient => vec![
            builtin_correlation_coefficient_table(),
            debris_hunter_correlation_coefficient_table(),
            melting_layer_correlation_coefficient_table(),
            correlation_coefficient_class_bands_table(),
        ],
        ColorTableFamily::DifferentialPhase => vec![
            builtin_differential_phase_table(),
            twilight_cyclic_differential_phase_table(),
            phase_bands_differential_phase_table(),
        ],
        ColorTableFamily::SpecificDifferentialPhase => vec![
            builtin_specific_differential_phase_table(),
            heavy_rain_specific_differential_phase_table(),
            fine_detail_specific_differential_phase_table(),
        ],
        ColorTableFamily::Generic => vec![builtin_generic_table()],
    }
}

pub fn analyst_reflectivity_table() -> ColorTable {
    banded_preset(
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
}

pub fn nws_reflectivity_table() -> ColorTable {
    parsed_preset("NWS Classic REF", NWS_CLASSIC_REFLECTIVITY_TABLE)
}

pub fn analyst_classic_reflectivity_table() -> ColorTable {
    parsed_preset("Analyst Classic REF", ANALYST_CLASSIC_REFLECTIVITY_TABLE)
}

pub fn gr2_reflectivity_table() -> ColorTable {
    parsed_preset("GR2Analyst Classic REF", GR2_REFLECTIVITY_TABLE)
}

pub fn storm_detail_reflectivity_table() -> ColorTable {
    parsed_preset("Analyst Storm Detail REF", STORM_DETAIL_REFLECTIVITY_TABLE)
}

pub fn hail_core_reflectivity_table() -> ColorTable {
    parsed_preset("Analyst Hail Core REF", HAIL_CORE_REFLECTIVITY_TABLE)
}

pub fn low_precip_reflectivity_table() -> ColorTable {
    parsed_preset("Analyst Low Precip REF", LOW_PRECIP_REFLECTIVITY_TABLE)
}

pub fn dark_scope_reflectivity_table() -> ColorTable {
    parsed_preset("Dark Scope REF", DARK_SCOPE_REFLECTIVITY_TABLE)
}

pub fn tornado_debris_reflectivity_table() -> ColorTable {
    parsed_preset("Tornado Debris REF", TORNADO_DEBRIS_REFLECTIVITY_TABLE)
}

pub fn clean_light_reflectivity_table() -> ColorTable {
    parsed_preset("Clean Light REF", CLEAN_LIGHT_REFLECTIVITY_TABLE)
}

// ---------------------------------------------------------------------------
// Continuously interpolated reflectivity
//
// Every reflectivity preset above this comment is stepped: each carries a
// `step:` row, so `sample` quantises the gate's dBZ onto a 2.5 or 5 dBZ grid
// before it looks up a colour. Inside a bin every gate paints the identical
// colour, which is why the display draws flat plateaus with hard edges between
// them. That is a deliberate reading aid - the edges are contours of constant
// reflectivity, and an analyst can count them - but it is also indistinguishable
// from a renderer that cannot interpolate. With nothing but stepped tables in
// the picker there was no way to tell the two apart from the scope.
//
// So these three are the same field seen the other way: no quantisation, and
// enough stops that the ramp reads as a gradient. Between them and the stepped
// presets, banding that survives a switch to an interpolated table is the
// renderer's, and banding that does not is the palette's.
//
// All three keep the four break points people actually read reflectivity by.
// Under the Marshall-Palmer relation Z = 200 R^1.6 (Marshall, J. S., and
// W. M. Palmer, 1948: "The distribution of raindrops with size", J. Meteor., 5,
// 165-166, doi:10.1175/1520-0469(1948)005<0165:TDORWS>2.0.CO;2) those dBZ
// values are rain rates that mean something different from each other:
//
// * 20 dBZ -> 0.65 mm/h. Precipitation onset; below it is drizzle, cloud, and
//   the clear-air return of insects and dust (Fabry, F., 2015: "Radar
//   Meteorology: Principles and Practice", Cambridge Univ. Press,
//   doi:10.1017/CBO9781107707405, ch. 8).
// * 35 dBZ -> 5.6 mm/h. Moderate-to-heavy rain; in convection this is the edge
//   of the core.
// * 50 dBZ -> 49 mm/h. Torrential rain or hail. Near the 45-50 dBZ thresholds
//   the operational hail algorithms are built on (Waldvogel, A., B. Federer,
//   and P. Grimm, 1979: "Criteria for the detection of hail cells", J. Appl.
//   Meteor., 18, 1521-1525,
//   doi:10.1175/1520-0450(1979)018<1521:CFTDOH>2.0.CO;2; Witt, A., and
//   coauthors, 1998: "An enhanced hail detection algorithm for the WSR-88D",
//   Wea. Forecasting, 13, 286-303,
//   doi:10.1175/1520-0434(1998)013<0286:AEHDAF>2.0.CO;2).
// * 65 dBZ -> 420 mm/h, which no rain shaft produces. At 65 dBZ the target is
//   large hail, and saying so is the whole point of the last band.
//
// A gradient that smears those four away is prettier than a stepped table and
// worse at the job, so all three place a half-dBZ turn on each one - one data
// step wide, since Level II reflectivity is quantised to 0.5 dBZ - and run a
// true gradient in between. The result is four thin contours where an analyst
// wants contours, and continuous tone everywhere else. Smooth Classic and
// Smooth Storm Core turn hue; Smooth Sequential steps luminance up, which is
// what lets it keep a monotone lightness ramp and still show the thresholds.
//
// All three also ink from 10 dBZ, exactly like the stepped presets, so
// switching between them changes how the echo is coloured. The half-dBZ alpha
// ramp below 10 dBZ is one Level II step wide; it exists because an
// interpolated table cannot have a discontinuity, not to fade anything in.
//
// On raw Level II values nothing lands inside that ramp: the 8-bit reflectivity
// word decodes as (raw - 66) / 2, so every value a radar can send sits exactly
// on the 0.5 dBZ grid, and 9.5 and 10.0 are both grid points. That is NOT true
// once the display passes run. `render2d::smooth` (a 3x3 binomial on the polar
// lattice) and `render2d::interpolate` (bilinear inter-gate upsampling) both
// produce physical values off the 0.5 grid, and a NaN-aware [1 2 1] pass along
// range over KABR 2026-08-18 06:43:14Z put 74,932 of 3,799,008 gates strictly
// inside 9.5 < dBZ < 10.0, and over KDMX 2026-08-18 08:34:01Z 59,827 of
// 5,025,389 - one to two percent, all of them at the outer fringe of the echo.
// Those gates draw at partial alpha on these three tables and fully clear on
// every quantised preset, whose `sample` returns transparent below its first
// opaque stop outright. So in Soften or Interpolate display modes an echo edge
// is one half-dBZ softer here than on a stepped table; the interior, which is
// what the palette is being judged on, is unaffected.
// `the_interpolated_reflectivity_presets_ink_the_same_gates_as_the_stepped_ones`
// pins both halves of that: identical on the 0.5 dBZ grid, half-alpha off it.
//
// At the top they run to 95 rather than the stepped presets' 92.5, because
// that same encoding tops out at (255 - 66) / 2 = 94.5 dBZ. The last stop is
// past the last value the field can hold, so nothing is ever clamped onto it
// and the legend's upper bound is a number the data can actually reach.
// ---------------------------------------------------------------------------

/// The operational hue sequence, continuously interpolated.
///
/// Blue for light echo, green for rain, yellow through orange for heavy rain,
/// red for a core, magenta for hail, white for the top of the scale - the same
/// order every stepped preset above uses, so nothing an analyst has learned to
/// read moves. What changes is that the 15 dBZ of green between 20 and 35 are
/// now 15 dBZ of *varying* green, and a gradient inside a storm is visible
/// instead of quantised into three plateaus.
///
/// This is the table to switch to first when the question is whether the
/// banding on the scope belongs to the palette or to the renderer.
pub fn smooth_classic_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Smooth Classic REF",
        vec![
            clear_stop(-10.0),
            clear_stop(9.5),
            stop(10.0, 16, 88, 140),
            stop(12.5, 18, 118, 176),
            stop(15.0, 20, 148, 208),
            stop(17.5, 24, 178, 230),
            stop(19.5, 40, 208, 244),
            // 20 dBZ: precipitation onset. Blue gives way to green.
            stop(20.0, 14, 148, 60),
            stop(22.5, 16, 168, 58),
            stop(25.0, 20, 190, 58),
            stop(27.5, 40, 206, 56),
            stop(30.0, 64, 214, 55),
            stop(32.5, 92, 220, 54),
            stop(34.5, 120, 224, 52),
            // 35 dBZ: moderate-to-heavy rain. Green gives way to yellow.
            stop(35.0, 250, 228, 36),
            stop(37.5, 251, 212, 32),
            stop(40.0, 252, 196, 28),
            stop(42.5, 252, 178, 26),
            stop(45.0, 251, 160, 24),
            stop(47.5, 250, 140, 22),
            stop(49.5, 249, 124, 20),
            // 50 dBZ: the core. Amber gives way to red.
            stop(50.0, 230, 18, 26),
            stop(52.5, 216, 14, 26),
            stop(55.0, 198, 10, 26),
            stop(57.5, 180, 8, 28),
            stop(60.0, 160, 8, 30),
            stop(62.5, 142, 8, 34),
            stop(64.5, 126, 8, 38),
            // 65 dBZ: not rain. Red gives way to magenta.
            stop(65.0, 214, 40, 200),
            stop(67.5, 226, 86, 220),
            stop(70.0, 232, 130, 234),
            stop(72.5, 222, 168, 240),
            stop(75.0, 226, 200, 246),
            stop(80.0, 238, 226, 250),
            stop(85.0, 246, 240, 252),
            stop(95.0, 255, 255, 255),
        ],
    )
}

/// Reflectivity on a ramp whose lightness only ever increases.
///
/// The classic radar hue order is not monotone in lightness: yellow at 40 dBZ
/// is far lighter than the dark red at 60, so a storm's strongest gates are
/// *darker* than its moderate ones and the eye has to be told the order rather
/// than seeing it. Worse for the job in hand, a ramp that goes light-dark-light
/// hides small gradients wherever it doubles back.
///
/// This one is built the way the colour-map literature says a sequential scale
/// should be: lightness rising monotonically end to end, hue carrying the rest
/// of the information (Kovesi, P., 2015: "Good colour maps: how to design
/// them", arXiv:1509.03700; Crameri, F., G. E. Shephard, and P. J. Heron, 2020:
/// "The misuse of colour in science communication", Nat. Commun., 11, 5444,
/// doi:10.1038/s41467-020-19160-x). Lightness is the BT.709 relative luminance
/// 0.2126 R + 0.7152 G + 0.0722 B, and a test checks it rises at every one of
/// the 32 inked stops.
///
/// Two consequences worth knowing before reaching for it. Strongest is always
/// brightest, so a core reads as a peak in a relief map rather than as a colour
/// to be looked up. And because lightness never doubles back, the palette
/// cannot stall: on a stepped table a plateau might be the palette's bin or the
/// renderer's, and here there is nowhere for the palette to plateau.
///
/// The four break points are turns here, exactly as on Smooth Classic REF, and
/// this was got wrong the first time. The original stops moved 3-8 units of
/// colour across each break window - *less* than the 8 units the widest
/// ordinary half-dBZ window moved, and at 20 dBZ less than the window
/// immediately before it - so 49.5 dBZ and 50.5 dBZ were the same orange and
/// the core boundary was invisible. Measured on KABR 2026-08-18 06:43:14Z and
/// KDMX 2026-08-18 08:34:01Z, whose reflectivity fields between them hold 119
/// and 123 distinct dBZ values above 10 dBZ.
///
/// Monotone lightness does not require a smooth ramp, only a rising one, so
/// each break is now a step *up*: luminance jumps 15 to 40 units in the half
/// dBZ below 20, 35, 50 and 65, against 1 to 5 units for an ordinary half-dBZ
/// window. Every break window moves at least twelve times as far as the worst
/// non-break window, and `the_contour_tables_turn_at_the_four_breaks_and_glide_between_them`
/// holds all three interpolated presets to the same bar.
///
/// The hue story is unchanged: indigo at the bottom, violet from 20, red from
/// 35, amber from 50, near-white from 65.
pub fn smooth_sequential_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Smooth Sequential REF",
        vec![
            clear_stop(-10.0),
            clear_stop(9.5),
            stop(10.0, 20, 10, 40),
            stop(12.5, 28, 13, 60),
            stop(15.0, 36, 16, 80),
            stop(17.5, 44, 19, 100),
            stop(19.5, 50, 21, 116),
            // 20 dBZ: precipitation onset. Indigo steps up into violet.
            stop(20.0, 98, 26, 144),
            stop(22.5, 110, 31, 148),
            stop(25.0, 122, 36, 150),
            stop(27.5, 134, 41, 151),
            stop(30.0, 146, 46, 150),
            stop(32.5, 158, 51, 148),
            stop(34.5, 168, 55, 145),
            // 35 dBZ: moderate-to-heavy rain. Violet steps up into red.
            stop(35.0, 226, 66, 66),
            stop(37.5, 233, 78, 58),
            stop(40.0, 238, 89, 50),
            stop(42.5, 243, 99, 44),
            stop(45.0, 247, 108, 39),
            stop(47.5, 250, 116, 35),
            stop(49.5, 252, 122, 32),
            // 50 dBZ: the core. Red steps up into amber.
            stop(50.0, 255, 178, 26),
            stop(52.5, 255, 184, 34),
            stop(55.0, 255, 190, 44),
            stop(57.5, 255, 196, 54),
            stop(60.0, 255, 201, 64),
            stop(62.5, 254, 206, 74),
            stop(64.5, 253, 210, 82),
            // 65 dBZ: not rain. Amber steps up into near-white.
            stop(65.0, 248, 248, 176),
            stop(67.5, 250, 250, 196),
            stop(70.0, 252, 251, 214),
            stop(75.0, 253, 253, 230),
            stop(80.0, 254, 254, 243),
            stop(95.0, 255, 255, 255),
        ],
    )
}

/// Reflectivity with the colour spent between 35 and 65 dBZ.
///
/// Interrogating a convective core on a full-range palette means most of the
/// scope's colour is being used on stratiform rain that is not the question.
/// This table holds everything below 35 dBZ in low-saturation slate and blue -
/// present, locatable, never competing - and spends the rest of its travel on
/// the 30 dBZ where a core, a hail shaft and a debris ball live.
///
/// The same idea the ZDR Column Hunter and Heavy Rain KDP presets apply to
/// their own moments: a preset earns its place when it is stretched over the
/// band one specific question lives in.
///
/// Interpolated, and with the same four break turns as Smooth Classic REF, so
/// the 50 dBZ contour is still a contour.
pub fn smooth_storm_core_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Smooth Storm Core REF",
        vec![
            clear_stop(-10.0),
            clear_stop(9.5),
            stop(10.0, 40, 44, 50),
            stop(12.5, 46, 50, 56),
            stop(15.0, 52, 56, 62),
            stop(17.5, 57, 61, 67),
            stop(19.5, 60, 64, 70),
            // 20 dBZ: precipitation onset, marked but kept muted. The turn is
            // into blue rather than up in brightness, so it is findable without
            // the sub-convective band ever competing with the core above.
            stop(20.0, 50, 74, 110),
            stop(22.5, 54, 79, 117),
            stop(25.0, 58, 84, 124),
            stop(27.5, 62, 89, 131),
            stop(30.0, 66, 94, 138),
            stop(32.5, 70, 99, 145),
            stop(34.5, 74, 104, 152),
            // 35 dBZ: the palette switches on.
            stop(35.0, 24, 152, 78),
            stop(37.5, 46, 182, 66),
            stop(40.0, 104, 204, 58),
            stop(42.5, 168, 218, 54),
            stop(45.0, 226, 224, 50),
            stop(47.5, 246, 190, 42),
            stop(49.5, 250, 178, 40),
            // 50 dBZ: the core.
            stop(50.0, 248, 80, 30),
            stop(52.5, 244, 54, 34),
            stop(55.0, 236, 24, 38),
            stop(57.5, 220, 16, 46),
            stop(60.0, 202, 12, 56),
            stop(62.5, 184, 10, 66),
            stop(64.5, 168, 10, 76),
            // 65 dBZ: hail.
            stop(65.0, 226, 60, 214),
            stop(67.5, 236, 118, 230),
            stop(70.0, 242, 166, 240),
            stop(72.5, 246, 204, 246),
            stop(75.0, 250, 232, 250),
            stop(80.0, 252, 244, 252),
            stop(95.0, 255, 255, 255),
        ],
    )
}

pub fn analyst_velocity_table() -> ColorTable {
    parsed_preset("Analyst Pro VEL", ANALYST_PRO_VELOCITY_TABLE)
}

pub fn nws_velocity_table() -> ColorTable {
    parsed_preset("NWS Classic VEL", NWS_VELOCITY_TABLE)
}

pub fn gr2_velocity_table() -> ColorTable {
    parsed_preset("GR2Analyst Classic VEL", GR2_VELOCITY_TABLE)
}

pub fn tight_couplet_velocity_table() -> ColorTable {
    parsed_preset("Analyst Tight Couplet VEL", TIGHT_COUPLET_VELOCITY_TABLE)
}

pub fn radarscope_contrast_velocity_table() -> ColorTable {
    parsed_preset(
        "RadarScope Contrast VEL",
        RADARSCOPE_CONTRAST_VELOCITY_TABLE,
    )
}

pub fn sign_check_velocity_table() -> ColorTable {
    parsed_preset("Sign Check VEL", SIGN_CHECK_VELOCITY_TABLE)
}

pub fn couplet_pop_velocity_table() -> ColorTable {
    parsed_preset("Couplet Pop VEL", COUPLET_POP_VELOCITY_TABLE)
}

pub fn gr2_ish_analyst_velocity_table() -> ColorTable {
    parsed_preset("GR2-ish Analyst VEL", GR2_ISH_ANALYST_VELOCITY_TABLE)
}

pub fn subtle_srv_velocity_table() -> ColorTable {
    parsed_preset("Subtle SRV VEL", SUBTLE_SRV_VELOCITY_TABLE)
}

pub fn nws_split_velocity_table() -> ColorTable {
    parsed_preset("NWS Split VEL", NWS_SPLIT_VELOCITY_TABLE)
}

pub fn dark_analyst_velocity_table() -> ColorTable {
    parsed_preset("Dark Analyst VEL", DARK_ANALYST_VELOCITY_TABLE)
}

// ---------------------------------------------------------------------------
// Continuously interpolated velocity
//
// Same argument as the interpolated reflectivity block above, and it bites
// harder here. Every velocity preset above quantises onto a 1 or 2 m/s grid,
// and the thing an analyst is looking for in a velocity field is a *gradient*:
// a couplet is two adjacent gates of opposite sign, and its strength is the
// difference between them. A quantised palette rounds that difference to the
// bin size before it is ever drawn, so a 3 m/s shear and a 4 m/s shear can
// paint the same two colours.
//
// Both tables here keep the conventions that make a velocity display readable:
// negative is inbound and runs green through cyan, positive is outbound and
// runs red through amber, and zero is a neutral grey so the zero isodop is a
// line rather than a colour. Sign is not a magnitude - an analyst reads the
// two halves as different things - so the two run to different hues rather
// than to two ends of one ramp.
// ---------------------------------------------------------------------------

/// Doppler velocity, continuously interpolated over the full +/-70 m/s.
///
/// The everyday table: the familiar green-inbound, red-outbound scheme with no
/// quantisation, ramping out to near-white at both extremes so a strong core of
/// either sign is unmistakable. Colour is spread over the whole domain rather
/// than concentrated, which is what you want when the question is the flow
/// field - a rear-inflow jet, a low-level jet, the breadth of an outflow - and
/// not one small rotation.
pub fn smooth_doppler_velocity_table() -> ColorTable {
    smooth_preset(
        "Smooth Doppler VEL",
        vec![
            stop(-70.0, 236, 255, 255),
            stop(-62.0, 198, 248, 255),
            stop(-55.0, 150, 238, 252),
            stop(-48.0, 100, 224, 246),
            stop(-42.0, 56, 206, 236),
            stop(-36.0, 24, 186, 216),
            stop(-32.0, 16, 196, 172),
            stop(-28.0, 16, 208, 136),
            stop(-24.0, 18, 218, 96),
            stop(-20.0, 22, 228, 62),
            stop(-16.0, 20, 202, 58),
            stop(-12.0, 18, 174, 54),
            stop(-8.0, 16, 146, 50),
            stop(-5.0, 34, 128, 60),
            stop(-3.0, 60, 116, 74),
            stop(-1.5, 92, 110, 96),
            stop(0.0, 112, 112, 112),
            stop(1.5, 130, 100, 98),
            stop(3.0, 148, 84, 78),
            stop(5.0, 168, 62, 60),
            stop(8.0, 190, 40, 42),
            stop(12.0, 212, 28, 32),
            stop(16.0, 232, 24, 28),
            stop(20.0, 250, 26, 26),
            stop(24.0, 252, 66, 22),
            stop(28.0, 253, 100, 20),
            stop(32.0, 254, 132, 20),
            stop(36.0, 255, 162, 26),
            stop(42.0, 255, 196, 48),
            stop(48.0, 255, 220, 96),
            stop(55.0, 255, 236, 150),
            stop(62.0, 255, 246, 200),
            stop(70.0, 255, 252, 240),
        ],
    )
}

/// Doppler velocity with the colour spent inside +/-25 m/s.
///
/// A tornadic or mesocyclonic couplet is defined by its rotational velocity,
/// half the inbound-to-outbound difference across the circulation, and the
/// operational thresholds sit low: the WSR-88D mesocyclone detection algorithm
/// works from shear and momentum over circulations whose rotational velocities
/// are typically 15-25 m/s (Stumpf, G. J., and coauthors, 1998: "The National
/// Severe Storms Laboratory mesocyclone detection algorithm for the WSR-88D",
/// Wea. Forecasting, 13, 304-326,
/// doi:10.1175/1520-0434(1998)013<0304:TNSSLM>2.0.CO;2). Most base-velocity
/// data is inside the Nyquist interval anyway, which for the common precipitation
/// VCPs is nearer 25-32 m/s than 70.
///
/// So this table gives 36% of its domain - the +/-25 m/s that couplets live in -
/// about 63% of its colour travel, with hue anchors on 15 and 25 m/s of both
/// signs. Beyond 25 m/s it keeps changing, just more slowly, so a dealiasing
/// failure or a genuine 60 m/s gate is still visibly extreme.
///
/// Interpolated, which is the point: the gate-to-gate difference across a
/// couplet is drawn at the resolution the data has rather than rounded to a
/// 1 m/s bin first.
pub fn smooth_couplet_velocity_table() -> ColorTable {
    smooth_preset(
        "Smooth Couplet VEL",
        vec![
            stop(-70.0, 214, 250, 250),
            stop(-55.0, 170, 244, 250),
            stop(-45.0, 120, 236, 248),
            stop(-38.0, 70, 226, 244),
            stop(-32.0, 24, 214, 238),
            // -25 m/s: strong inbound.
            stop(-25.0, 0, 206, 214),
            stop(-22.0, 8, 214, 170),
            stop(-19.0, 12, 222, 124),
            // -15 m/s: mesocyclone-strength inbound.
            stop(-15.0, 18, 232, 70),
            stop(-12.0, 16, 210, 62),
            stop(-9.0, 14, 186, 56),
            stop(-6.0, 26, 158, 56),
            stop(-4.0, 44, 134, 62),
            stop(-2.0, 70, 112, 82),
            stop(-1.0, 90, 102, 96),
            stop(0.0, 104, 104, 104),
            stop(1.0, 118, 96, 94),
            stop(2.0, 138, 86, 82),
            stop(4.0, 166, 62, 60),
            stop(6.0, 194, 42, 42),
            stop(9.0, 218, 28, 30),
            stop(12.0, 240, 22, 26),
            // +15 m/s: mesocyclone-strength outbound.
            stop(15.0, 255, 44, 28),
            stop(19.0, 255, 96, 24),
            stop(22.0, 255, 142, 22),
            // +25 m/s: strong outbound.
            stop(25.0, 255, 188, 28),
            stop(32.0, 255, 214, 88),
            stop(38.0, 255, 228, 136),
            stop(45.0, 255, 240, 184),
            stop(55.0, 255, 248, 218),
            stop(70.0, 255, 253, 244),
        ],
    )
}

pub fn builtin_spectrum_width_table() -> ColorTable {
    smooth_preset(
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
}

/// Turbulence-hunting spectrum width, stretched across 4-12 m/s.
///
/// Spectrum width is the spread of the Doppler spectrum in one resolution
/// volume, so it rises with shear and turbulence inside the beam. The default
/// preset above spends most of its ramp on the 0-8 m/s bulk of a scan; this one
/// pushes the ramp into the band where a mesocyclone, a gust front, or a
/// three-body scatter spike separates from ordinary precipitation.
pub fn turbulence_spectrum_width_table() -> ColorTable {
    smooth_preset(
        "Turbulence SW",
        vec![
            stop(0.0, 12, 14, 22),
            stop(2.0, 18, 34, 62),
            stop(4.0, 26, 78, 132),
            stop(5.0, 28, 130, 176),
            stop(6.0, 34, 176, 156),
            stop(7.0, 96, 202, 96),
            stop(8.0, 188, 216, 60),
            stop(9.0, 240, 200, 50),
            stop(10.0, 246, 150, 42),
            stop(11.0, 240, 92, 40),
            stop(12.0, 228, 40, 48),
            stop(16.0, 198, 46, 156),
            stop(24.0, 246, 246, 250),
        ],
    )
}

/// Clear-air spectrum width, stretched across 0-6 m/s.
///
/// Boundaries, fine lines, and bird/insect returns in a clear-air VCP sit under
/// about 6 m/s, where the default preset has barely left its first two stops.
pub fn clear_air_spectrum_width_table() -> ColorTable {
    smooth_preset(
        "Clear Air SW",
        vec![
            stop(0.0, 10, 26, 46),
            stop(0.5, 18, 56, 96),
            stop(1.0, 24, 92, 148),
            stop(1.5, 28, 132, 182),
            stop(2.0, 32, 172, 176),
            stop(2.5, 48, 196, 124),
            stop(3.0, 116, 208, 78),
            stop(3.5, 188, 216, 62),
            stop(4.0, 238, 210, 54),
            stop(5.0, 246, 156, 44),
            stop(6.0, 240, 88, 40),
            stop(10.0, 200, 40, 80),
            stop(16.0, 150, 44, 150),
            stop(24.0, 240, 240, 246),
        ],
    )
}

/// Spectrum width as flat categories rather than a ramp.
///
/// Stepped, so every gate inside a band paints one colour and the band edges
/// read as contours. Useful when the question is "where does the field cross
/// 8 m/s", not "how does it vary".
pub fn spectrum_width_class_bands_table() -> ColorTable {
    banded_preset(
        "SW Class Bands",
        vec![
            stop(0.0, 20, 32, 56),
            stop(2.0, 32, 104, 168),
            stop(4.0, 40, 168, 140),
            stop(6.0, 150, 206, 66),
            stop(8.0, 240, 206, 52),
            stop(11.0, 240, 122, 42),
            stop(14.0, 226, 44, 52),
            stop(18.0, 176, 48, 168),
            stop(24.0, 240, 240, 248),
        ],
    )
}

// ---------------------------------------------------------------------------
// Dual-polarimetric palettes
//
// Interpretation breaks below are taken from Kumjian, M. R. (2013):
// "Principles and applications of dual-polarization weather radar",
// J. Operational Meteor., Part I 1(19), 226-242, doi:10.15191/nwajom.2013.0119;
// Part II 1(20), 243-264, doi:10.15191/nwajom.2013.0120;
// Part III 1(21), 265-274, doi:10.15191/nwajom.2013.0121.
// Physical ranges follow Ryzhkov, A. V., and D. S. Zrnic (2019):
// "Radar Polarimetry for Weather Observations", Springer,
// doi:10.1007/978-3-030-05093-1.
//
// Two design rules are shared by all of them.
//
// First: put colour travel where the physical discrimination is, not where the
// numeric range is. A linear ramp over the declared domain is the failure mode
// this module exists to fix.
//
// Second, and learned the hard way from the cached Level II volumes: a palette
// must span the whole range its field can encode, and it must not spend its
// brightest colour on the part of that range which is instrument noise. Every
// value past the last stop is painted in the last stop's colour, so a domain
// that stops short of the field's own saturation code turns that code's
// pile-up into a flat wash - the very defect the dual-pol families were added
// to fix. Both ZDR and RHOHV pile up hard on their top code in real data, so
// both palettes end muted rather than white.
// ---------------------------------------------------------------------------

/// Declared ZDR domain, taken from the field's own encoding rather than from a
/// round number.
///
/// The Level II 16-bit ZDR word carries scale 32 and offset 418, and codes 0
/// and 1 are reserved for "below threshold" and "range folded", so the field
/// runs from (2 - 418)/32 = -13.0 dB to (1058 - 418)/32 = +20.0 dB. Decoding
/// KUEX, KABR, KTLX, KLTX and KDMX confirms exactly that: every volume reports
/// scale 32, offset 418, raw minimum 2 and raw maximum 1058.
///
/// The palette earlier stopped at +8 dB on the theory that the field saturates
/// near +/-7.9 dB, which is the *legacy 8-bit* product's range, not this one's.
/// On real scans 3.4% (KABR) to 25.0% (KLTX) of all gates sit at or above
/// +8 dB - biological scatterers, sea clutter and low-SNR noise - and every one
/// of them was clamped onto the palette's brightest colour.
const ZDR_MIN_DB: f32 = -13.0;
const ZDR_MAX_DB: f32 = 20.0;

/// Where the *meteorological* ZDR scale ends, which is not where the field
/// ends. Rain tops out near 5 dB, the melting layer and the largest oblate
/// drops near 7-8 dB (Kumjian 2013 Part I); past that, ZDR is biota, clutter or
/// noise. Colour is spent between these two bounds; outside them the palettes
/// run to a muted off-scale band so junk cannot outshine weather.
const ZDR_MET_MIN_DB: f32 = -7.0;
const ZDR_MET_MAX_DB: f32 = 8.0;

/// Declared RHOHV domain, taken from the field's own encoding.
///
/// The WSR-88D 8-bit RHOHV word carries scale 300 and offset -60.5 with codes 0
/// and 1 reserved, so the smallest and largest values it can hold are
/// (2 + 60.5)/300 = 0.2083 and (255 + 60.5)/300 = 1.0517. Written as those
/// quotients so the constants are exactly the decoded endpoints - `MomentGrid`
/// decodes as `(raw - offset) / scale` over the same f32 values - and no real
/// gate can fall outside the palette.
///
/// Code 255 is a saturation code, not a measurement: across the five cached
/// volumes it holds 4.1% to 10.3% of *all* gates but only 0.012% to 0.037% of
/// gates with reflectivity above 20 dBZ, and the median reflectivity of the
/// gates carrying it is -2.5 to +7.5 dBZ. RHOHV above unity is not physical for
/// a single hydrometeor population; it is the low-SNR bias of the estimator
/// (Ryzhkov and Zrnic 2019, ch. 3). So the palettes peak at 1.00 and darken
/// into the ceiling instead of ending white.
const CC_MIN: f32 = 62.5 / 300.0;
const CC_MAX: f32 = 315.5 / 300.0;

/// Declared PHIDP domain. The field wraps: 359 deg and 1 deg are two degrees
/// apart, not 358.
const PHI_MIN_DEG: f32 = 0.0;
const PHI_MAX_DEG: f32 = 360.0;

/// Declared KDP domain, in deg/km.
const KDP_MIN_DEG_PER_KM: f32 = -2.0;
const KDP_MAX_DEG_PER_KM: f32 = 7.0;

pub fn builtin_differential_reflectivity_table() -> ColorTable {
    analyst_differential_reflectivity_table()
}

/// Differential reflectivity with the three bands a forecaster reads.
///
/// ZDR is the log ratio of horizontal to vertical reflectivity, so it measures
/// how oblate the scatterers are (Kumjian 2013 Part I, section 3). The breaks:
///
/// * Near zero, -0.5 to +0.3 dB: spherical or tumbling scatterers - dry hail,
///   large hail that is falling chaotically, dry snow aggregates. Held on one
///   neutral hue across the whole band - it brightens from (124,124,128) to
///   (176,176,180) but never leaves grey - so the band reads as one category
///   while still resolving where inside it a gate sits. This is the band that
///   pairs with high reflectivity to say "hail", and the band that pairs with
///   low CC to say "debris".
/// * 1 to 3 dB: rain. Drops flatten as they grow, so ZDR climbs with drop size
///   through this range. Green through yellow.
/// * Above 4 dB: large oblate drops - the melting band, drop-size sorting on
///   the storm's forward flank, and biological scatterers. Orange through red
///   into magenta so it separates hard from the rain band below it.
///
/// Negative ZDR runs violet. It is uncommon and worth noticing: vertically
/// aligned ice in a strong electric field, or a bad calibration.
///
/// Outside -7 to +8 dB the palette leaves the meteorological scale and runs to
/// a dark teal that appears nowhere else in the table, so off-scale echo stays
/// legible without competing with weather for attention.
pub fn analyst_differential_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Analyst ZDR",
        vec![
            stop(ZDR_MIN_DB, 34, 0, 64),
            stop(ZDR_MET_MIN_DB, 58, 10, 92),
            stop(-4.0, 92, 26, 148),
            stop(-2.0, 120, 66, 196),
            stop(-1.0, 96, 122, 208),
            // Grey plateau: both ends of the near-zero band are neutral, so the
            // band changes only in lightness and no hue creeps into it.
            stop(-0.5, 124, 124, 128),
            stop(0.3, 176, 176, 180),
            stop(0.4, 24, 96, 62),
            stop(0.7, 26, 140, 74),
            stop(1.0, 44, 188, 86),
            stop(1.5, 132, 210, 70),
            stop(2.0, 198, 224, 64),
            stop(2.5, 240, 216, 58),
            stop(3.0, 248, 170, 44),
            stop(3.5, 246, 124, 36),
            stop(4.0, 238, 62, 44),
            stop(5.0, 208, 30, 98),
            stop(6.0, 198, 46, 172),
            stop(7.0, 228, 152, 228),
            stop(ZDR_MET_MAX_DB, 246, 246, 250),
            stop(9.0, 88, 168, 176),
            stop(14.0, 34, 96, 104),
            stop(ZDR_MAX_DB, 16, 40, 46),
        ],
    )
}

/// The same ZDR breaks as flat categories.
///
/// Stepped, so each interpretation band from Kumjian (2013, Part I) paints one
/// colour and the band edges become contours. Reach for this when the question
/// is which category a core falls in rather than how ZDR varies inside it.
///
/// "At or above 8 dB" is itself one of those categories - non-meteorological -
/// so it gets a band of its own rather than the top of the ramp, and the
/// encoding ceiling at 20 dB gets one more so a saturated field is visible as
/// saturated.
pub fn storm_interrogation_differential_reflectivity_table() -> ColorTable {
    banded_preset(
        "Storm Interrogation ZDR",
        vec![
            stop(ZDR_MIN_DB, 30, 6, 54),
            stop(ZDR_MET_MIN_DB, 72, 20, 110),
            stop(-1.0, 86, 106, 178),
            stop(-0.5, 112, 112, 116),
            stop(0.3, 30, 120, 70),
            stop(1.0, 46, 190, 88),
            stop(2.0, 206, 222, 62),
            stop(3.0, 250, 176, 44),
            stop(4.0, 238, 66, 46),
            stop(5.0, 206, 34, 120),
            stop(6.0, 200, 48, 176),
            stop(ZDR_MET_MAX_DB, 56, 124, 130),
            stop(ZDR_MAX_DB, 26, 62, 68),
        ],
    )
}

/// ZDR stretched onto 0.5-4 dB to find ZDR columns.
///
/// A ZDR column is a plume of ZDR above 1 dB extending above the environmental
/// 0 C level, where supercooled raindrops are being lofted; it marks the updraft
/// and it leads hail and tornadogenesis (Kumjian, M. R., A. P. Khain,
/// N. BenMoshe, E. Ilotoviz, A. V. Ryzhkov, and V. T. J. Phillips, 2014: "The
/// anatomy and physics of ZDR columns", J. Appl. Meteor. Climatol., 53,
/// 1820-1843, doi:10.1175/JAMC-D-13-0354.1). The bright end of this palette
/// therefore sits on 2.5-5 dB; below 0.5 dB it is near-black and past 5 dB it
/// darkens again, so a column is what the eye lands on. That was not true
/// before: the palette ran to white at its top stop, handing the brightest
/// colour on the scope to the biological scatterers and sea clutter that hold
/// 3-25% of the gates in a real volume.
pub fn zdr_column_hunter_table() -> ColorTable {
    smooth_preset(
        "ZDR Column Hunter",
        vec![
            stop(ZDR_MIN_DB, 8, 8, 12),
            stop(ZDR_MET_MIN_DB, 10, 10, 16),
            stop(0.5, 14, 18, 30),
            stop(1.0, 26, 70, 120),
            stop(1.5, 32, 132, 168),
            stop(2.0, 44, 186, 150),
            stop(2.5, 130, 214, 92),
            stop(3.0, 232, 216, 62),
            stop(3.5, 246, 146, 44),
            stop(4.0, 236, 52, 48),
            stop(5.0, 232, 108, 200),
            stop(ZDR_MET_MAX_DB, 96, 60, 110),
            stop(ZDR_MAX_DB, 20, 14, 28),
        ],
    )
}

/// ZDR stretched onto -1 to +1 dB, so the near-zero band is the bright one.
///
/// Large hail depolarises so little that ZDR collapses to zero regardless of
/// how big the stones are, which is why a hail core reads as high reflectivity
/// with ZDR near 0 (Kumjian 2013 Part II, section 3). Every other ZDR value is
/// pushed dark here so the hail signal, and the near-zero ZDR of a tornadic
/// debris signature, is what the eye lands on.
pub fn hail_signal_differential_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Hail Signal ZDR",
        vec![
            stop(ZDR_MIN_DB, 16, 4, 28),
            stop(ZDR_MET_MIN_DB, 28, 6, 48),
            stop(-2.0, 54, 18, 96),
            stop(-1.0, 92, 60, 176),
            stop(-0.6, 60, 128, 210),
            stop(-0.3, 40, 190, 200),
            stop(-0.1, 250, 250, 250),
            stop(0.1, 250, 250, 250),
            stop(0.3, 240, 196, 60),
            stop(0.6, 230, 128, 46),
            stop(1.0, 206, 58, 48),
            stop(2.0, 120, 36, 60),
            stop(4.0, 56, 60, 96),
            stop(ZDR_MET_MAX_DB, 24, 30, 52),
            stop(ZDR_MAX_DB, 14, 18, 32),
        ],
    )
}

pub fn builtin_correlation_coefficient_table() -> ColorTable {
    analyst_correlation_coefficient_table()
}

/// Correlation coefficient on a deliberately non-linear scale.
///
/// This is the single most important design decision in this module. RHOHV is
/// bounded above by 1 and essentially all meteorological echo sits between 0.95
/// and 1.00 - a seventeenth of the 0.2083-1.0517 declared domain. A linear ramp
/// gives that seventeenth about 6% of its colour range, so rain, wet snow, and
/// the melting layer all come out the same colour and the field is decorative
/// rather than diagnostic.
///
/// So the stops are packed towards unity: five of the fifteen stops fall in
/// 0.95-1.00, which turns 6% of the domain into roughly 40% of the colour path.
/// The breaks follow Kumjian (2013, Part I, section 5):
///
/// * Above 0.97: meteorological, a single hydrometeor type filling the beam.
/// * 0.90-0.97: mixed-phase - the melting layer's bright-band dip, wet
///   aggregates, hail large enough to resonate.
/// * 0.80-0.90: mixed hydrometeors, big wet hail, the edges of non-met echo.
/// * Below 0.80: non-meteorological. Ground clutter, chaff, birds, insects, and
///   tornadic debris (Ryzhkov, A. V., T. J. Schuur, D. W. Burgess, and
///   D. S. Zrnic, 2005: "Polarimetric tornado detection", J. Appl. Meteor., 44,
///   557-570, doi:10.1175/JAM2235.1, which sets the debris threshold at 0.80
///   and notes most debris falls below 0.70).
///
/// The brightest colour is at 1.00, not at the top of the domain. Everything
/// above unity is the estimator's low-SNR bias rather than a measurement (see
/// `CC_MAX`), and the field's saturation code alone carries 4-10% of the gates
/// in a real volume, so the ramp dims into the ceiling.
pub fn analyst_correlation_coefficient_table() -> ColorTable {
    smooth_preset(
        "Analyst CC",
        vec![
            stop(CC_MIN, 36, 12, 60),
            stop(0.45, 78, 20, 96),
            stop(0.65, 128, 32, 96),
            stop(0.75, 186, 46, 74),
            stop(0.80, 226, 86, 48),
            stop(0.85, 240, 140, 40),
            stop(0.90, 246, 196, 52),
            stop(0.93, 206, 220, 62),
            stop(0.95, 120, 206, 84),
            stop(0.96, 56, 190, 120),
            stop(0.97, 30, 168, 168),
            stop(0.98, 34, 128, 200),
            stop(0.99, 56, 84, 216),
            stop(1.00, 150, 150, 235),
            stop(CC_MAX, 96, 100, 120),
        ],
    )
}

/// CC with the whole ramp below 0.90, for tornadic debris.
///
/// A tornadic debris signature is co-located high reflectivity, near-zero ZDR,
/// and RHOHV below 0.80 - usually below 0.70 (Ryzhkov et al. 2005,
/// doi:10.1175/JAM2235.1). This table burns its brightest colours there and
/// mutes everything meteorological, which inverts the usual reading: the debris
/// ball is the only lit thing on the scope.
pub fn debris_hunter_correlation_coefficient_table() -> ColorTable {
    smooth_preset(
        "Debris Hunter CC",
        vec![
            stop(CC_MIN, 255, 240, 120),
            stop(0.45, 250, 170, 40),
            stop(0.60, 240, 80, 40),
            stop(0.70, 226, 30, 90),
            stop(0.75, 198, 26, 150),
            stop(0.80, 140, 40, 190),
            stop(0.85, 76, 66, 190),
            stop(0.90, 40, 92, 150),
            stop(0.95, 26, 92, 110),
            stop(0.97, 30, 60, 78),
            stop(0.99, 46, 46, 52),
            stop(1.00, 60, 60, 66),
            stop(CC_MAX, 84, 84, 90),
        ],
    )
}

/// CC with the whole ramp inside 0.85-1.00, for the melting layer.
///
/// The bright band is a local RHOHV minimum, typically 0.90-0.97, sandwiched
/// between snow above and rain below that both sit above 0.98 (Giangrande,
/// S. E., J. M. Krause, and A. V. Ryzhkov, 2008: "Automatic designation of the
/// melting layer with a polarimetric prototype of the WSR-88D radar",
/// J. Appl. Meteor. Climatol., 47, 1354-1364, doi:10.1175/2007JAMC1634.1, which
/// designates the layer on a 0.90-0.97 RHOHV window). Spending eleven stops
/// above 0.85 makes that dip a band of its own colour instead of a shade; those
/// stops carry 82% of the table's colour travel across 24% of its domain.
///
/// Below 0.85 the table stays dark so the bright-band ramp is what the eye
/// lands on, but it still changes hue - plum, dark red, dark amber, dark slate -
/// rather than fading through one near-black gradient. A muted region is meant
/// to be de-emphasised, not made unreadable: a forecaster who glances at
/// non-meteorological echo on this table must still be able to tell 0.55 from
/// 0.75, and equal-luminance hue changes buy that without stealing attention.
pub fn melting_layer_correlation_coefficient_table() -> ColorTable {
    smooth_preset(
        "Melting Layer CC",
        vec![
            stop(CC_MIN, 34, 22, 40),
            stop(0.45, 110, 34, 56),
            stop(0.65, 96, 60, 24),
            stop(0.80, 26, 54, 78),
            stop(0.85, 40, 50, 110),
            stop(0.88, 34, 106, 176),
            stop(0.90, 30, 160, 176),
            stop(0.92, 54, 196, 118),
            stop(0.94, 150, 214, 66),
            stop(0.95, 216, 216, 54),
            stop(0.96, 246, 176, 44),
            stop(0.97, 244, 110, 40),
            stop(0.98, 226, 44, 56),
            stop(0.99, 176, 40, 130),
            stop(1.00, 150, 150, 220),
            stop(CC_MAX, 96, 100, 118),
        ],
    )
}

/// CC as flat hydrometeor-classification bands.
///
/// Stepped on the Kumjian (2013, Part I) breaks, so each gate paints the colour
/// of the category it falls in and the category edges become contours. This is
/// the quality-control view: everything below 0.80 is one colour, and it is the
/// colour you scan for before trusting a rainfall estimate.
///
/// The last band is the field's saturation code on its own, in slate rather
/// than the near-white it used to take, so a display full of low-SNR gates
/// looks like a display full of low-SNR gates.
pub fn correlation_coefficient_class_bands_table() -> ColorTable {
    banded_preset(
        "CC Class Bands",
        vec![
            stop(CC_MIN, 206, 44, 168),
            stop(0.70, 232, 96, 44),
            stop(0.80, 240, 200, 56),
            stop(0.90, 92, 196, 96),
            stop(0.95, 36, 158, 168),
            stop(0.97, 46, 96, 200),
            stop(1.00, 200, 210, 240),
            stop(CC_MAX, 104, 108, 124),
        ],
    )
}

pub fn builtin_differential_phase_table() -> ColorTable {
    analyst_differential_phase_table()
}

/// Differential phase on a closed hue wheel.
///
/// PHIDP is the accumulated phase difference between the H and V returns along
/// the beam. It only increases with range through precipitation, and it wraps:
/// past 360 deg the field folds back to 0 (Ryzhkov and Zrnic 2019, chapter 4).
/// A ramp with different colours at each end therefore draws a hard edge across
/// every wrapping ray, and an analyst reads that edge as a real gradient when
/// it is an artefact of the number line.
///
/// The fix is a cyclic map - one whose first and last colours are identical, so
/// there is no privileged point on the scale (Kovesi, P., 2015: "Good colour
/// maps: how to design them", arXiv:1509.03700, section 4). This is a constant
/// saturation, constant value hue wheel sampled every 30 deg, so 360 deg
/// returns exactly the colour of 0 deg and the wrap is invisible.
pub fn analyst_differential_phase_table() -> ColorTable {
    smooth_preset(
        "Analyst Cyclic PHI",
        vec![
            stop(PHI_MIN_DEG, 242, 36, 36),
            stop(30.0, 242, 139, 36),
            stop(60.0, 242, 242, 36),
            stop(90.0, 139, 242, 36),
            stop(120.0, 36, 242, 36),
            stop(150.0, 36, 242, 139),
            stop(180.0, 36, 242, 242),
            stop(210.0, 36, 139, 242),
            stop(240.0, 36, 36, 242),
            stop(270.0, 139, 36, 242),
            stop(300.0, 242, 36, 242),
            stop(330.0, 242, 36, 139),
            stop(PHI_MAX_DEG, 242, 36, 36),
        ],
    )
}

/// Differential phase on a closed dark-to-light-to-dark cycle.
///
/// The hue wheel above is cyclic but is not monotone in lightness, so a mid-grey
/// display or a colour-vision deficiency can flatten parts of it. This one
/// cycles lightness instead of hue - pale lavender through blue to near-black
/// and back through red - which keeps the wrap closed while giving the eye a
/// brightness gradient to follow. Same construction as matplotlib's `twilight`,
/// designed to the Kovesi (2015, arXiv:1509.03700) cyclic criteria.
pub fn twilight_cyclic_differential_phase_table() -> ColorTable {
    smooth_preset(
        "Twilight Cyclic PHI",
        vec![
            stop(PHI_MIN_DEG, 226, 217, 226),
            stop(36.0, 150, 180, 225),
            stop(72.0, 72, 132, 205),
            stop(108.0, 36, 84, 158),
            stop(144.0, 30, 44, 96),
            stop(180.0, 34, 26, 46),
            stop(216.0, 96, 34, 58),
            stop(252.0, 160, 50, 60),
            stop(288.0, 206, 96, 78),
            stop(324.0, 222, 160, 150),
            stop(PHI_MAX_DEG, 226, 217, 226),
        ],
    )
}

/// Differential phase as 15 deg isophase bands.
///
/// Stepped, so the display draws contours of constant PHIDP. What matters
/// operationally is the range derivative - KDP is half of it - and band spacing
/// reads a derivative far better than a smooth ramp does: bands crowd together
/// where phase accumulates fast. Still cyclic: the band at 345-360 deg abuts the
/// band at 0-15 deg exactly one band-step away in colour, the same step as
/// every other boundary, so the wrap is not a special edge.
pub fn phase_bands_differential_phase_table() -> ColorTable {
    banded_preset(
        "Phase Bands PHI",
        vec![
            stop(PHI_MIN_DEG, 242, 36, 36),
            stop(15.0, 242, 88, 36),
            stop(30.0, 242, 139, 36),
            stop(45.0, 242, 191, 36),
            stop(60.0, 242, 242, 36),
            stop(75.0, 191, 242, 36),
            stop(90.0, 139, 242, 36),
            stop(105.0, 88, 242, 36),
            stop(120.0, 36, 242, 36),
            stop(135.0, 36, 242, 88),
            stop(150.0, 36, 242, 139),
            stop(165.0, 36, 242, 191),
            stop(180.0, 36, 242, 242),
            stop(195.0, 36, 191, 242),
            stop(210.0, 36, 139, 242),
            stop(225.0, 36, 88, 242),
            stop(240.0, 36, 36, 242),
            stop(255.0, 88, 36, 242),
            stop(270.0, 139, 36, 242),
            stop(285.0, 191, 36, 242),
            stop(300.0, 242, 36, 242),
            stop(315.0, 242, 36, 191),
            stop(330.0, 242, 36, 139),
            stop(345.0, 242, 36, 88),
            stop(PHI_MAX_DEG, 242, 36, 36),
        ],
    )
}

pub fn builtin_specific_differential_phase_table() -> ColorTable {
    analyst_specific_differential_phase_table()
}

/// Specific differential phase, diverging about zero.
///
/// KDP is half the range derivative of PHIDP, so it is a local measure of how
/// much liquid the beam is passing through: immune to attenuation, partial beam
/// blockage, and absolute calibration, which is why it carries rainfall
/// estimation (Ryzhkov and Zrnic 2019, chapter 6). Sign is meaningful - negative
/// KDP means vertically aligned scatterers or non-uniform beam filling, not
/// "less rain" - so zero is the neutral grey pivot and the two signs run to
/// different hues rather than to two ends of one ramp.
///
/// Breaks follow Kumjian (2013, Part I, section 4): below about 0.5 deg/km is
/// light rain or ice, 0.5-2 is moderate to heavy rain, and above 2 is very heavy
/// rain or a rain/hail mix.
pub fn analyst_specific_differential_phase_table() -> ColorTable {
    smooth_preset(
        "Analyst KDP",
        vec![
            stop(KDP_MIN_DEG_PER_KM, 40, 0, 80),
            stop(-1.0, 78, 30, 150),
            stop(-0.5, 60, 90, 180),
            stop(-0.25, 86, 110, 140),
            stop(0.0, 112, 112, 112),
            stop(0.25, 60, 120, 90),
            stop(0.5, 30, 160, 80),
            stop(1.0, 90, 200, 60),
            stop(1.5, 180, 220, 56),
            stop(2.0, 240, 220, 50),
            stop(3.0, 246, 168, 40),
            stop(4.0, 240, 96, 36),
            stop(5.0, 226, 40, 44),
            stop(6.0, 208, 44, 150),
            stop(KDP_MAX_DEG_PER_KM, 245, 240, 250),
        ],
    )
}

/// KDP stretched onto 0.5-4 deg/km, the heavy rain band.
///
/// R(KDP) relations are the operational rainfall estimator inside convection
/// because KDP does not care about hail contamination or attenuation
/// (Ryzhkov and Zrnic 2019, chapter 6). This table darkens everything under
/// 0.5 deg/km so the heavy-rain core is what stands out, which is the view for
/// flash-flood interrogation rather than for storm structure.
pub fn heavy_rain_specific_differential_phase_table() -> ColorTable {
    smooth_preset(
        "Heavy Rain KDP",
        vec![
            stop(KDP_MIN_DEG_PER_KM, 14, 16, 24),
            stop(0.0, 20, 24, 36),
            stop(0.5, 26, 62, 110),
            stop(1.0, 28, 120, 170),
            stop(1.5, 36, 176, 150),
            stop(2.0, 110, 208, 92),
            stop(2.5, 214, 218, 62),
            stop(3.0, 248, 170, 44),
            stop(3.5, 244, 108, 38),
            stop(4.0, 230, 42, 48),
            stop(5.0, 206, 44, 152),
            stop(KDP_MAX_DEG_PER_KM, 250, 250, 252),
        ],
    )
}

/// KDP stretched onto -0.5 to +1.5 deg/km.
///
/// Outside a convective core KDP is small and noisy, and the interesting
/// structure - ice crystal alignment, the KDP foot below the melting layer,
/// weak stratiform rain - lives in a range where the default table has moved
/// three stops. Diverging about zero like the default so sign still reads.
pub fn fine_detail_specific_differential_phase_table() -> ColorTable {
    smooth_preset(
        "KDP Fine Detail",
        vec![
            stop(KDP_MIN_DEG_PER_KM, 48, 8, 70),
            stop(-1.0, 86, 26, 140),
            stop(-0.5, 92, 92, 200),
            stop(-0.25, 70, 150, 214),
            stop(-0.1, 96, 150, 160),
            stop(0.0, 118, 118, 118),
            stop(0.1, 104, 154, 96),
            stop(0.25, 48, 176, 76),
            stop(0.5, 128, 208, 60),
            stop(0.75, 206, 222, 56),
            stop(1.0, 246, 202, 48),
            stop(1.25, 248, 146, 40),
            stop(1.5, 240, 74, 44),
            stop(3.0, 188, 40, 118),
            stop(KDP_MAX_DEG_PER_KM, 244, 236, 248),
        ],
    )
}

pub fn builtin_generic_table() -> ColorTable {
    smooth_preset(
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
}

fn stop(value: f32, r: u8, g: u8, b: u8) -> ColorStop {
    ColorStop {
        value,
        color: Rgba8::opaque(r, g, b),
    }
}

/// A stop that paints nothing, used to hold the bottom of a palette clear.
///
/// The parsed reflectivity presets do this with `color4: ... 0` rows; a table
/// built from stops needs the same thing so that a scan's noise floor stays off
/// the scope instead of covering it in the first stop's colour, which is what
/// `sample` would otherwise do for every value below the first stop.
fn clear_stop(value: f32) -> ColorStop {
    ColorStop {
        value,
        color: Rgba8::TRANSPARENT,
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

        assert_eq!(table.name(), "GR2Analyst Classic REF (quantized stepped)");
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

        assert_eq!(table.name(), "Analyst Tornado VEL (quantized stepped)");
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
                "GR2Analyst Classic REF (quantized stepped)",
                "Smooth Classic REF (interpolated)",
                "Smooth Sequential REF (interpolated)",
                "Smooth Storm Core REF (interpolated)",
                "Analyst Classic REF (quantized stepped)",
                "NWS Classic REF (quantized stepped)",
                "Dark Scope REF (quantized stepped)",
                "Analyst Hail Core REF (quantized stepped)",
                "Analyst Low Precip REF (quantized stepped)",
                "Tornado Debris REF (quantized stepped)",
                "Clean Light REF (quantized stepped)",
            ]
        );
        assert_eq!(
            velocity,
            vec![
                "Analyst Tornado VEL (quantized stepped)",
                "Smooth Doppler VEL (interpolated)",
                "Smooth Couplet VEL (interpolated)",
                "Analyst Pro VEL (stepped)",
                "RadarScope Contrast VEL (quantized stepped)",
                "Sign Check VEL (stepped)",
                "Couplet Pop VEL (quantized stepped)",
                "GR2-ish Analyst VEL (quantized stepped)",
                "Subtle SRV VEL (quantized stepped)",
            ]
        );
    }

    /// Both families an analyst spends the day in must offer the field drawn
    /// both ways, or there is no way to tell a bad palette from a bad renderer.
    ///
    /// Counted rather than merely listed: the lists above pin the exact names,
    /// this pins the property those names have to satisfy.
    #[test]
    fn reflectivity_and_velocity_each_offer_stepped_and_interpolated_tables() {
        for (family, minimum_interpolated) in [
            (ColorTableFamily::Reflectivity, 3),
            (ColorTableFamily::Velocity, 2),
        ] {
            let tables = builtin_tables_for_family(family);
            let interpolated = tables.iter().filter(|table| table.interpolates()).count();
            let stepped = tables.len() - interpolated;
            assert!(
                interpolated >= minimum_interpolated,
                "{} offers {interpolated} interpolated tables, wanted {minimum_interpolated}",
                family.label()
            );
            assert!(stepped >= 1, "{} lost its stepped tables", family.label());
        }
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

        assert_eq!(table.name(), "Sign Check VEL (stepped)");
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

    #[test]
    fn the_reflectivity_preset_inks_from_ten_dbz_not_from_its_transparent_first_stop() {
        let table = builtin_reflectivity_table();

        // The GR2 preset declares stops at -10 and 7.5 dBZ with alpha 0, so a
        // legend drawn across the declared domain would label empty scope.
        assert_eq!(table.stops()[0].value, -10.0);
        assert_eq!(table.stops()[0].color.a, 0);
        assert_eq!(table.inked_value_span(), Some((10.0, 92.5)));
        // Tie the reported low bound to what actually gets painted.
        assert_eq!(table.sample(7.5), Rgba8::TRANSPARENT);
        assert_ne!(table.sample(10.0), Rgba8::TRANSPARENT);
    }

    #[test]
    fn a_table_with_no_transparent_stops_inks_its_whole_first_to_last_span() {
        let table = builtin_generic_table();

        assert_eq!(table.stops().first().expect("has stops").value, 0.0);
        assert_eq!(table.stops().last().expect("has stops").value, 100.0);
        assert_eq!(table.inked_value_span(), Some((0.0, 100.0)));
    }

    #[test]
    fn an_entirely_transparent_table_reports_no_inked_span_so_its_legend_is_suppressed() {
        let table = ColorTable::parse(
            "Blank sample",
            r#"
            product: BR
            units: dBZ
            color4: -30 0 0 0 0
            color4: 0 0 0 0 0
            color4: 30 0 0 0 0
            "#,
        )
        .expect("table parses");

        assert_eq!(table.inked_value_span(), None);
    }

    #[test]
    fn transparent_stops_at_both_ends_are_trimmed_so_only_the_middle_is_reported() {
        let table = ColorTable::parse(
            "Trimmed sample",
            r#"
            product: BR
            units: dBZ
            color4: -20 0 0 0 0
            color4: -5 0 0 0 0
            color: 5 0 0 100
            color: 45 200 0 0
            color4: 60 0 0 0 0
            color4: 80 0 0 0 0
            "#,
        )
        .expect("table parses");

        assert_eq!(table.inked_value_span(), Some((5.0, 45.0)));
    }

    #[test]
    fn the_range_folded_color_does_not_widen_the_inked_span() {
        let table = ColorTable::parse(
            "Folded sample",
            r#"
            product: BV
            units: m/s
            RF: 200 40 240
            color4: -50 0 0 0 0
            color: -20 0 0 255
            color: 20 255 0 0
            color4: 50 0 0 0 0
            "#,
        )
        .expect("table parses");

        // The folded color is opaque, but it is keyed off the folded code rather
        // than a velocity, so the span must stay at the inked stops.
        assert_eq!(table.range_folded_rgba(), Rgba8::new(200, 40, 240, 245));
        assert_eq!(table.inked_value_span(), Some((-20.0, 20.0)));
    }

    /// Every built-in table paired with the inked span its stops imply, read
    /// off the palette text by hand rather than from `inked_value_span` itself.
    ///
    /// Forty-five of the forty-seven carry their palette values through
    /// `parse` untouched. Two do not: `nws_velocity_table` declares
    /// `units: kt` and `vortex_velocity_table` declares `scale: 2.237`, so
    /// `parse` multiplies every stop into m/s. Their expectations are written
    /// as that same product, not as a copied decimal, so a change to
    /// `KNOT_TO_MPS` moves the test with the code instead of silently failing.
    fn every_builtin_table_with_expected_span() -> Vec<(&'static str, ColorTable, (f32, f32))> {
        vec![
            (
                "analyst_reflectivity_table",
                analyst_reflectivity_table(),
                (-10.0, 75.0),
            ),
            (
                "nws_reflectivity_table",
                nws_reflectivity_table(),
                (10.0, 92.5),
            ),
            (
                "analyst_classic_reflectivity_table",
                analyst_classic_reflectivity_table(),
                (10.0, 92.5),
            ),
            (
                "gr2_reflectivity_table",
                gr2_reflectivity_table(),
                (10.0, 92.5),
            ),
            (
                "storm_detail_reflectivity_table",
                storm_detail_reflectivity_table(),
                (5.0, 80.0),
            ),
            (
                "hail_core_reflectivity_table",
                hail_core_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "low_precip_reflectivity_table",
                low_precip_reflectivity_table(),
                (10.0, 90.0),
            ),
            (
                "dark_scope_reflectivity_table",
                dark_scope_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "tornado_debris_reflectivity_table",
                tornado_debris_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "clean_light_reflectivity_table",
                clean_light_reflectivity_table(),
                (10.0, 92.5),
            ),
            (
                "smooth_classic_reflectivity_table",
                smooth_classic_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "smooth_sequential_reflectivity_table",
                smooth_sequential_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "smooth_storm_core_reflectivity_table",
                smooth_storm_core_reflectivity_table(),
                (10.0, 95.0),
            ),
            (
                "tornado_velocity_table",
                tornado_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "vortex_velocity_table",
                vortex_velocity_table(),
                (-130.0 / 2.237, 130.0 / 2.237),
            ),
            (
                "analyst_velocity_table",
                analyst_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "nws_velocity_table",
                nws_velocity_table(),
                (-120.0 * KNOT_TO_MPS, 120.0 * KNOT_TO_MPS),
            ),
            ("gr2_velocity_table", gr2_velocity_table(), (-70.0, 70.0)),
            (
                "tight_couplet_velocity_table",
                tight_couplet_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "radarscope_contrast_velocity_table",
                radarscope_contrast_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "sign_check_velocity_table",
                sign_check_velocity_table(),
                (-100.0, 100.0),
            ),
            (
                "couplet_pop_velocity_table",
                couplet_pop_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "gr2_ish_analyst_velocity_table",
                gr2_ish_analyst_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "subtle_srv_velocity_table",
                subtle_srv_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "smooth_doppler_velocity_table",
                smooth_doppler_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "smooth_couplet_velocity_table",
                smooth_couplet_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "nws_split_velocity_table",
                nws_split_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "dark_analyst_velocity_table",
                dark_analyst_velocity_table(),
                (-70.0, 70.0),
            ),
            (
                "builtin_spectrum_width_table",
                builtin_spectrum_width_table(),
                (0.0, 24.0),
            ),
            (
                "turbulence_spectrum_width_table",
                turbulence_spectrum_width_table(),
                (0.0, 24.0),
            ),
            (
                "clear_air_spectrum_width_table",
                clear_air_spectrum_width_table(),
                (0.0, 24.0),
            ),
            (
                "spectrum_width_class_bands_table",
                spectrum_width_class_bands_table(),
                (0.0, 24.0),
            ),
            (
                "analyst_differential_reflectivity_table",
                analyst_differential_reflectivity_table(),
                (-13.0, 20.0),
            ),
            (
                "storm_interrogation_differential_reflectivity_table",
                storm_interrogation_differential_reflectivity_table(),
                (-13.0, 20.0),
            ),
            (
                "zdr_column_hunter_table",
                zdr_column_hunter_table(),
                (-13.0, 20.0),
            ),
            (
                "hail_signal_differential_reflectivity_table",
                hail_signal_differential_reflectivity_table(),
                (-13.0, 20.0),
            ),
            (
                "analyst_correlation_coefficient_table",
                analyst_correlation_coefficient_table(),
                (62.5 / 300.0, 315.5 / 300.0),
            ),
            (
                "debris_hunter_correlation_coefficient_table",
                debris_hunter_correlation_coefficient_table(),
                (62.5 / 300.0, 315.5 / 300.0),
            ),
            (
                "melting_layer_correlation_coefficient_table",
                melting_layer_correlation_coefficient_table(),
                (62.5 / 300.0, 315.5 / 300.0),
            ),
            (
                "correlation_coefficient_class_bands_table",
                correlation_coefficient_class_bands_table(),
                (62.5 / 300.0, 315.5 / 300.0),
            ),
            (
                "analyst_differential_phase_table",
                analyst_differential_phase_table(),
                (0.0, 360.0),
            ),
            (
                "twilight_cyclic_differential_phase_table",
                twilight_cyclic_differential_phase_table(),
                (0.0, 360.0),
            ),
            (
                "phase_bands_differential_phase_table",
                phase_bands_differential_phase_table(),
                (0.0, 360.0),
            ),
            (
                "analyst_specific_differential_phase_table",
                analyst_specific_differential_phase_table(),
                (-2.0, 7.0),
            ),
            (
                "heavy_rain_specific_differential_phase_table",
                heavy_rain_specific_differential_phase_table(),
                (-2.0, 7.0),
            ),
            (
                "fine_detail_specific_differential_phase_table",
                fine_detail_specific_differential_phase_table(),
                (-2.0, 7.0),
            ),
            (
                "builtin_generic_table",
                builtin_generic_table(),
                (0.0, 100.0),
            ),
        ]
    }

    #[test]
    fn every_builtin_tables_inked_span_is_the_one_its_stops_imply() {
        // 1e-4 in table units. The two unit-scaled tables reach about 61.7 m/s,
        // where an f32 resolves roughly 4e-6, so this is two orders looser than
        // representation noise. It is also four orders tighter than the 1 m/s
        // finest built-in velocity bin and the 2.5 dBZ finest reflectivity
        // step, so no mis-scaled or off-by-one-stop bound can slip through.
        const TOLERANCE: f32 = 1e-4;

        for (label, table, expected) in every_builtin_table_with_expected_span() {
            let span = table
                .inked_value_span()
                .unwrap_or_else(|| panic!("{label} inks nothing"));
            assert!(
                (span.0 - expected.0).abs() <= TOLERANCE,
                "{label} low bound: got {}, expected {}",
                span.0,
                expected.0
            );
            assert!(
                (span.1 - expected.1).abs() <= TOLERANCE,
                "{label} high bound: got {}, expected {}",
                span.1,
                expected.1
            );
        }
    }

    #[test]
    fn exactly_twelve_builtin_tables_open_with_two_transparent_stops() {
        let tables = every_builtin_table_with_expected_span();
        assert_eq!(
            tables.len(),
            47,
            "every built-in constructor must be listed here or its span is unpinned"
        );

        let mut lead_in_transparent = 0;
        for (label, table, _) in &tables {
            if table.stops()[0].color.a > 0 {
                continue;
            }
            lead_in_transparent += 1;
            // Always two, never one, and never an interior or trailing hole:
            // the doc comment on inked_value_span states this shape, and a
            // palette edit that breaks it must break this test too.
            let transparent = table
                .stops()
                .iter()
                .filter(|stop| stop.color.a == 0)
                .count();
            assert_eq!(
                transparent, 2,
                "{label} should carry exactly two alpha-0 stops, all of them leading"
            );
        }
        assert_eq!(
            lead_in_transparent, 12,
            "twelve built-ins open transparent: the nine parsed reflectivity presets \
             and the three interpolated ones"
        );
    }

    /// A lead-in transparent preset must paint at the bound its legend
    /// advertises and paint nothing below the clear stop it declares.
    ///
    /// The gap between those two is where the modes differ, and it is why this
    /// is not one assertion for all twelve. A stepped or quantized table jumps:
    /// its last clear stop and its first inked stop are 2.5 or 5 dBZ apart and
    /// everything in between is fully clear, because `sample` refuses any value
    /// below the first opaque stop outright. An interpolated table cannot jump -
    /// interpolating from alpha 0 to alpha 255 is what it does for a living -
    /// so the three smooth presets put their last clear stop 0.5 dBZ below the
    /// first inked one and ramp alpha across that gap. Level II reflectivity is
    /// quantised to 0.5 dBZ (scale 2, offset 66), so the ramp is exactly one
    /// data step wide: at most one value per gate can land half-painted, which
    /// is the narrowest an interpolated table can make it.
    ///
    /// Both shapes are held to the same two facts, and the gap is capped at one
    /// bin so no preset can quietly widen it into a visible haze.
    #[test]
    fn each_lead_in_transparent_preset_paints_at_its_low_bound_and_stays_clear_below_it() {
        let mut checked = 0;
        for (label, table, _) in every_builtin_table_with_expected_span() {
            if table.stops()[0].color.a > 0 {
                continue;
            }
            let (low, _) = table
                .inked_value_span()
                .unwrap_or_else(|| panic!("{label} inks nothing"));
            let last_clear = table
                .stops()
                .iter()
                .filter(|stop| stop.color.a == 0)
                .map(|stop| stop.value)
                .fold(f32::NEG_INFINITY, f32::max);

            assert_eq!(
                table.sample(low).a,
                255,
                "{label} must paint fully opaque at its reported low bound {low}"
            );
            assert_eq!(
                table.sample(last_clear).a,
                0,
                "{label} must paint nothing at its last clear stop {last_clear}"
            );
            assert_eq!(
                table.sample(last_clear - 0.001).a,
                0,
                "{label} must paint nothing below its last clear stop {last_clear}"
            );
            assert!(
                low - last_clear > 0.0 && low - last_clear <= 5.0,
                "{label} leaves {} dBZ between its last clear stop and its first \
                 inked one, which is more than one bin",
                low - last_clear
            );
            checked += 1;
        }
        assert_eq!(checked, 12, "expected twelve lead-in transparent presets");
    }

    #[test]
    fn a_palette_with_a_single_inked_stop_reports_a_zero_width_span_not_none() {
        let table = ColorTable::parse(
            "Single inked stop",
            "color4: -10 0 0 0 0\ncolor: 10 255 0 0\n",
        )
        .expect("table parses");

        // from_parts guarantees two stops, not two inked stops. None would be
        // wrong here because the table does ink, at exactly one value; the
        // legend caller is the one that has to notice high == low before it
        // divides by the width and gets NaN tick positions.
        assert_eq!(table.inked_value_span(), Some((10.0, 10.0)));
    }

    #[test]
    fn mirroring_a_preset_moves_its_transparent_stops_to_the_top_of_the_span() {
        let mirrored = gr2_reflectivity_table().mirrored_values("Mirrored GR2");

        // mirrored_values negates every stop and from_parts re-sorts, so the
        // two alpha-0 stops that led the table now trail it. This is the only
        // trailing-transparent shape reachable from a built-in, and it proves
        // the scan tracks the last inked stop instead of assuming the final
        // stop is inked.
        assert_eq!(mirrored.stops().last().expect("has stops").value, 10.0);
        assert_eq!(mirrored.stops().last().expect("has stops").color.a, 0);
        assert_eq!(mirrored.inked_value_span(), Some((-92.5, -10.0)));
    }

    /// Largest per-channel difference between two colours, alpha ignored.
    ///
    /// A crude stand-in for perceptual distance, picked because it can be
    /// checked by hand straight off the stop lists above. Two colours that
    /// differ by 40 here are unambiguously different on a scope; two that
    /// differ by 1 are the same colour.
    fn max_channel_delta(left: Rgba8, right: Rgba8) -> i32 {
        let [left_r, left_g, left_b, _] = left.to_array();
        let [right_r, right_g, right_b, _] = right.to_array();
        [
            (left_r as i32 - right_r as i32).abs(),
            (left_g as i32 - right_g as i32).abs(),
            (left_b as i32 - right_b as i32).abs(),
        ]
        .into_iter()
        .max()
        .expect("three channels")
    }

    /// How far a table travels through colour space between two values.
    ///
    /// Summed as city-block distance over a fine sweep, which is what a fine
    /// sweep converges to: between adjacent samples at most one channel moves
    /// and it moves by one, so the total is the summed absolute variation of
    /// each channel. That makes the result comparable to a by-hand sum of
    /// |dR|+|dG|+|dB| across the stop list, which is how the expected values in
    /// these tests were derived.
    fn colour_path(table: &ColorTable, low: f32, high: f32, steps: usize) -> f64 {
        let span = (high - low) as f64;
        let mut total = 0.0;
        let mut previous = table.sample(low);
        for index in 1..=steps {
            let value = low as f64 + span * index as f64 / steps as f64;
            let current = table.sample(value as f32);
            let [previous_r, previous_g, previous_b, _] = previous.to_array();
            let [current_r, current_g, current_b, _] = current.to_array();
            total += (current_r as i32 - previous_r as i32).unsigned_abs() as f64
                + (current_g as i32 - previous_g as i32).unsigned_abs() as f64
                + (current_b as i32 - previous_b as i32).unsigned_abs() as f64;
            previous = current;
        }
        total
    }

    /// Every correlation coefficient value a forecaster reads a category off,
    /// paired with the category. Used to check tables separate them.
    const CC_CATEGORY_PROBES: [(f32, &str); 6] = [
        (0.55, "debris / chaff"),
        (0.75, "non-meteorological"),
        (0.87, "mixed hydrometeors"),
        (0.93, "melting layer"),
        (0.96, "marginal"),
        (0.99, "meteorological"),
    ];

    #[test]
    fn every_family_lists_tables_and_the_dual_pol_families_list_at_least_three() {
        for family in ColorTableFamily::ALL {
            let tables = builtin_tables_for_family(family);
            assert!(
                !tables.is_empty(),
                "{} enumerates no tables, so a picker would show an empty list",
                family.label()
            );
        }

        for family in [
            ColorTableFamily::SpectrumWidth,
            ColorTableFamily::DifferentialReflectivity,
            ColorTableFamily::CorrelationCoefficient,
            ColorTableFamily::DifferentialPhase,
            ColorTableFamily::SpecificDifferentialPhase,
        ] {
            assert!(
                builtin_tables_for_family(family).len() >= 3,
                "{} must offer at least three tables",
                family.label()
            );
        }
    }

    #[test]
    fn no_two_built_in_tables_anywhere_share_a_name() {
        let mut seen = Vec::new();
        for family in ColorTableFamily::ALL {
            for table in builtin_tables_for_family(family) {
                let name = table.name().to_owned();
                assert!(
                    !seen.contains(&name),
                    "{name} is listed twice; a picker cannot tell the two apart"
                );
                seen.push(name);
            }
        }
    }

    #[test]
    fn every_family_default_is_the_first_table_the_picker_offers() {
        let set = ColorTableSet::default();
        for family in ColorTableFamily::ALL {
            let first = builtin_tables_for_family(family)
                .into_iter()
                .next()
                .expect("every family lists a table");
            assert_eq!(
                set.for_family(family).name(),
                first.name(),
                "{} default disagrees with the head of its list",
                family.label()
            );
        }
    }

    #[test]
    fn each_dual_pol_family_is_stored_and_read_back_independently() {
        let mut set = ColorTableSet::default();
        set.set_family(
            ColorTableFamily::CorrelationCoefficient,
            debris_hunter_correlation_coefficient_table(),
        );

        assert_eq!(
            set.for_family(ColorTableFamily::CorrelationCoefficient)
                .name(),
            "Debris Hunter CC (interpolated)"
        );
        // Setting one dual-pol family must not disturb its neighbours, which
        // shared a single slot before they had families of their own.
        assert_eq!(
            set.for_family(ColorTableFamily::DifferentialReflectivity)
                .name(),
            "Analyst ZDR (interpolated)"
        );
        assert_eq!(
            set.for_family(ColorTableFamily::DifferentialPhase).name(),
            "Analyst Cyclic PHI (interpolated)"
        );
        assert_eq!(
            set.for_family(ColorTableFamily::SpecificDifferentialPhase)
                .name(),
            "Analyst KDP (interpolated)"
        );
        assert_eq!(
            set.for_family(ColorTableFamily::Generic).name(),
            "Analyst Generic (interpolated)"
        );
    }

    #[test]
    fn every_table_in_a_family_is_drawn_over_that_familys_declared_domain() {
        for family in ColorTableFamily::ALL {
            // The pre-existing families predate nominal_domain and each carries
            // presets on its own declared span; they are pinned table by table
            // in every_builtin_table_with_expected_span instead.
            if matches!(
                family,
                ColorTableFamily::Reflectivity
                    | ColorTableFamily::Velocity
                    | ColorTableFamily::Generic
            ) {
                continue;
            }
            let (low, high) = family.nominal_domain();
            for table in builtin_tables_for_family(family) {
                let span = table
                    .inked_value_span()
                    .unwrap_or_else(|| panic!("{} inks nothing", table.name()));
                assert!(
                    (span.0 - low).abs() < 1e-4 && (span.1 - high).abs() < 1e-4,
                    "{} spans {span:?} but its family declares ({low}, {high})",
                    table.name()
                );
            }
        }
    }

    #[test]
    fn the_dual_pol_domains_are_the_physical_ones_not_a_zero_to_hundred_ramp() {
        assert_eq!(
            ColorTableFamily::DifferentialReflectivity.nominal_domain(),
            (-13.0, 20.0)
        );
        assert_eq!(
            ColorTableFamily::CorrelationCoefficient.nominal_domain(),
            (62.5 / 300.0, 315.5 / 300.0)
        );
        assert_eq!(
            ColorTableFamily::DifferentialPhase.nominal_domain(),
            (0.0, 360.0)
        );
        assert_eq!(
            ColorTableFamily::SpecificDifferentialPhase.nominal_domain(),
            (-2.0, 7.0)
        );
        assert!(ColorTableFamily::DifferentialPhase.is_cyclic());
        for family in ColorTableFamily::ALL {
            if family != ColorTableFamily::DifferentialPhase {
                assert!(!family.is_cyclic(), "{} does not wrap", family.label());
            }
        }
    }

    /// The defect this module was changed to fix, pinned so it cannot come back.
    ///
    /// The generic ramp runs 0 to 100 over eight stops. Correlation coefficient
    /// occupies 0.2083 to 1.0517 of that, which lands entirely inside the first
    /// stop interval, 0 to 10. Reading the stop values by hand: the segment
    /// carries (34,40,64) to (34,82,130), so at 0.2083 it is (34,41,65) and at
    /// 1.0517 it is (34,44,71) - a total city-block travel of 9 against the
    /// table's full travel of 712, or 1.3%. The whole dual-pol field renders as
    /// one colour.
    #[test]
    fn the_generic_ramp_gives_the_whole_correlation_coefficient_domain_one_percent_of_its_colour() {
        let generic = builtin_generic_table();

        assert_eq!(generic.sample(CC_MIN), Rgba8::opaque(34, 41, 65));
        assert_eq!(generic.sample(CC_MAX), Rgba8::opaque(34, 44, 71));

        let over_cc_domain = colour_path(&generic, CC_MIN, CC_MAX, 20_000);
        let over_whole_table = colour_path(&generic, 0.0, 100.0, 200_000);
        assert!((over_cc_domain - 9.0).abs() < 0.5, "got {over_cc_domain}");
        assert!(
            (over_whole_table - 712.0).abs() < 2.0,
            "got {over_whole_table}"
        );
        assert!(over_cc_domain / over_whole_table < 0.02);

        // And the acid test: 0.95 against 0.99 is one unit in one channel.
        assert!(max_channel_delta(generic.sample(0.95), generic.sample(0.99)) <= 2);
    }

    /// The central design decision of the correlation coefficient palette.
    ///
    /// 0.95 to 1.00 is 5.9% of the 0.2083-1.0517 domain and carries essentially
    /// all meteorological echo. Summing |dR|+|dG|+|dB| across the stop list by
    /// hand gives 1462 total travel and 549 of it inside 0.95-1.00, so that
    /// 5.9% of the domain gets 37.6% of the colour - a factor of six.
    #[test]
    fn correlation_coefficient_concentrates_its_colour_where_the_echo_is() {
        let table = analyst_correlation_coefficient_table();

        let whole = colour_path(&table, CC_MIN, CC_MAX, 85_000);
        let meteorological = colour_path(&table, 0.95, 1.00, 5_000);
        assert!((whole - 1462.0).abs() < 4.0, "whole path {whole}");
        assert!(
            (meteorological - 549.0).abs() < 4.0,
            "0.95-1.00 path {meteorological}"
        );

        let fraction = meteorological / whole;
        assert!(
            (0.32..0.45).contains(&fraction),
            "0.95-1.00 should take about 38% of the colour, got {fraction}"
        );
        // The domain fraction it is being compared against: 253/300 wide.
        assert!((0.05_f64 / (253.0 / 300.0) - 0.0593).abs() < 0.001);
    }

    /// The acid test named in the brief: 0.95 and 0.99 are a melting layer and
    /// clean rain, and no correlation coefficient table may render them alike.
    #[test]
    fn every_correlation_coefficient_table_separates_point_nine_five_from_point_nine_nine() {
        for table in builtin_tables_for_family(ColorTableFamily::CorrelationCoefficient) {
            let delta = max_channel_delta(table.sample(0.95), table.sample(0.99));
            assert!(
                delta >= 40,
                "{} renders CC 0.95 and 0.99 only {delta} apart",
                table.name()
            );
        }
    }

    #[test]
    fn every_correlation_coefficient_table_separates_all_six_interpretation_categories() {
        for table in builtin_tables_for_family(ColorTableFamily::CorrelationCoefficient) {
            for (left_index, (left_value, left_label)) in CC_CATEGORY_PROBES.iter().enumerate() {
                for (right_value, right_label) in CC_CATEGORY_PROBES.iter().skip(left_index + 1) {
                    let delta =
                        max_channel_delta(table.sample(*left_value), table.sample(*right_value));
                    assert!(
                        delta >= 30,
                        "{} renders {left_label} ({left_value}) and {right_label} ({right_value}) only {delta} apart",
                        table.name()
                    );
                }
            }
        }
    }

    /// Hand-read off the Analyst ZDR stop list: 0 dB interpolates 62.5% of the
    /// way from (124,124,128) at -0.5 dB to (176,176,180) at +0.3 dB, giving
    /// (157,157,161) - a neutral grey, distinct from the rain band's greens and
    /// the melting band's reds.
    #[test]
    fn differential_reflectivity_paints_the_near_zero_band_neutral_and_the_rain_band_green() {
        let table = analyst_differential_reflectivity_table();

        assert_eq!(table.sample(0.0), Rgba8::opaque(157, 157, 161));
        let [zero_r, zero_g, zero_b, _] = table.sample(0.0).to_array();
        assert!((zero_r as i32 - zero_g as i32).abs() <= 6);
        assert!((zero_g as i32 - zero_b as i32).abs() <= 6);

        // 1 to 3 dB, the rain band, runs green into yellow: green dominates blue
        // by a wide margin at both ends.
        assert_eq!(table.sample(1.0), Rgba8::opaque(44, 188, 86));
        assert_eq!(table.sample(2.0), Rgba8::opaque(198, 224, 64));
        for value in [1.0_f32, 1.5, 2.0, 2.5] {
            let [red, green, blue, _] = table.sample(value).to_array();
            assert!(
                green as i32 - blue as i32 >= 80,
                "{value} dB should stay in the green-yellow rain band, got {red},{green},{blue}"
            );
        }

        // Above 4 dB - large drops, melting hail - is red into magenta, never
        // confusable with the grey band or the rain band.
        assert_eq!(table.sample(4.0), Rgba8::opaque(238, 62, 44));
        assert_eq!(table.sample(6.0), Rgba8::opaque(198, 46, 172));
        for value in [4.0_f32, 5.0, 6.0] {
            let [red, green, _, _] = table.sample(value).to_array();
            assert!(red > 190 && green < 90, "{value} dB should be red/magenta");
        }

        assert!(max_channel_delta(table.sample(0.0), table.sample(2.0)) >= 60);
        assert!(max_channel_delta(table.sample(2.0), table.sample(5.0)) >= 60);
        assert!(max_channel_delta(table.sample(0.0), table.sample(5.0)) >= 60);
    }

    /// Analyst ZDR is drawn over the field's whole -13 to +20 dB encoding but
    /// real ZDR clusters in -1 to +5, so the palette is weighted there. Summing
    /// |dR|+|dG|+|dB| off the stop list gives 2780 total: 62 below -7, 2078
    /// across the meteorological -7 to +8 scale, and 640 above it. 1338 of that
    /// sits inside -1 to +5, so 18% of the domain takes 48% of the colour.
    /// ZDR Column Hunter is weighted harder still, 870 of 1594 - 55% of the
    /// colour - inside 0.5 to 4 dB, which is 11% of the domain.
    ///
    /// The meteorological figure of 2078 is unchanged from when the palette
    /// stopped at +/-7-8 dB: widening the domain added stops outside that
    /// window and moved none inside it, so every gate that was already on scale
    /// still samples the identical colour.
    #[test]
    fn differential_reflectivity_tables_weight_the_range_the_data_actually_occupies() {
        let analyst = analyst_differential_reflectivity_table();
        let whole = colour_path(&analyst, ZDR_MIN_DB, ZDR_MAX_DB, 400_000);
        let meteorological = colour_path(&analyst, ZDR_MET_MIN_DB, ZDR_MET_MAX_DB, 200_000);
        let operational = colour_path(&analyst, -1.0, 5.0, 80_000);
        assert!((whole - 2780.0).abs() < 8.0, "whole path {whole}");
        assert!(
            (meteorological - 2078.0).abs() < 8.0,
            "-7..8 path {meteorological}"
        );
        assert!(
            (operational - 1338.0).abs() < 8.0,
            "-1..5 path {operational}"
        );
        assert!(operational / whole >= 0.45);

        let column = zdr_column_hunter_table();
        let column_whole = colour_path(&column, ZDR_MIN_DB, ZDR_MAX_DB, 400_000);
        let column_band = colour_path(&column, 0.5, 4.0, 80_000);
        assert!((column_whole - 1594.0).abs() < 8.0, "whole {column_whole}");
        assert!(
            (column_band - 870.0).abs() < 8.0,
            "0.5..4 path {column_band}"
        );
        assert!(column_band / column_whole >= 0.50);
    }

    /// The hail signature is high reflectivity with ZDR pinned near zero, so
    /// this table's brightest colour is the near-zero plateau and everything
    /// else darkens away from it.
    #[test]
    fn the_hail_signal_table_makes_near_zero_the_brightest_thing_on_the_scope() {
        let table = hail_signal_differential_reflectivity_table();

        assert_eq!(table.sample(0.0), Rgba8::opaque(250, 250, 250));
        let brightness = |value: f32| {
            let [red, green, blue, _] = table.sample(value).to_array();
            red as i32 + green as i32 + blue as i32
        };
        let near_zero = brightness(0.0);
        for value in [-3.0_f32, -1.0, 1.0, 3.0, 6.0] {
            assert!(
                brightness(value) < near_zero - 150,
                "{value} dB should sit well below the near-zero plateau"
            );
        }
    }

    /// PHIDP wraps, so a table whose ends disagree draws a false edge along
    /// every ray that folds past 360 deg.
    #[test]
    fn every_differential_phase_table_closes_on_itself_at_the_wrap() {
        for table in builtin_tables_for_family(ColorTableFamily::DifferentialPhase) {
            assert_eq!(
                table.sample(0.0),
                table.sample(360.0),
                "{} does not close: 0 and 360 deg are different colours",
                table.name()
            );
        }
    }

    /// Closure alone is not enough: the step taken across the wrap must be no
    /// larger than the steps taken anywhere else, or the wrap still reads as an
    /// edge. Checked against the largest step the table takes over any interval
    /// of the same width elsewhere in its domain.
    #[test]
    fn the_phase_wrap_is_no_sharper_than_any_other_two_degrees_of_the_scale() {
        const PROBE_HALF_WIDTH: f32 = 1.0;

        for table in builtin_tables_for_family(ColorTableFamily::DifferentialPhase) {
            let wrap = max_channel_delta(
                table.sample(360.0 - PROBE_HALF_WIDTH),
                table.sample(PROBE_HALF_WIDTH),
            );

            let mut worst_interior = 0;
            let mut centre = PROBE_HALF_WIDTH;
            while centre <= 360.0 - PROBE_HALF_WIDTH {
                worst_interior = worst_interior.max(max_channel_delta(
                    table.sample(centre - PROBE_HALF_WIDTH),
                    table.sample(centre + PROBE_HALF_WIDTH),
                ));
                centre += 0.5;
            }

            assert!(
                wrap <= worst_interior,
                "{} jumps {wrap} across the wrap but at most {worst_interior} anywhere else",
                table.name()
            );
        }
    }

    /// Hand-read off the Analyst Cyclic PHI stop list: 359 deg is 96.67% of the
    /// way from (242,36,139) to (242,36,36), giving (242,36,39); 1 deg is 3.33%
    /// of the way from (242,36,36) to (242,139,36), giving (242,39,36). Three
    /// units apart, which is invisible on a scope - the point of a cyclic map.
    #[test]
    fn the_cyclic_phase_hues_meet_within_three_units_across_the_fold() {
        let table = analyst_differential_phase_table();

        assert_eq!(table.sample(359.0), Rgba8::opaque(242, 36, 39));
        assert_eq!(table.sample(1.0), Rgba8::opaque(242, 39, 36));
        assert!(max_channel_delta(table.sample(359.0), table.sample(1.0)) <= 3);

        // A quarter turn apart must still be plainly different, or the map has
        // closed itself by going nowhere.
        assert!(max_channel_delta(table.sample(0.0), table.sample(90.0)) >= 100);
        assert!(max_channel_delta(table.sample(90.0), table.sample(180.0)) >= 100);
        assert!(max_channel_delta(table.sample(180.0), table.sample(270.0)) >= 100);
    }

    /// KDP's sign is physical, not a magnitude, so zero is a pivot rather than
    /// a point on a ramp.
    #[test]
    fn specific_differential_phase_diverges_about_a_neutral_zero() {
        let table = analyst_specific_differential_phase_table();

        assert_eq!(table.sample(0.0), Rgba8::opaque(112, 112, 112));
        let [zero_r, zero_g, zero_b, _] = table.sample(0.0).to_array();
        assert_eq!(zero_r, zero_g);
        assert_eq!(zero_g, zero_b);

        // Negative runs blue/violet, positive runs green through yellow to red.
        let [negative_r, negative_g, negative_b, _] = table.sample(-1.0).to_array();
        assert!(negative_b > negative_r && negative_b > negative_g);
        let [positive_r, positive_g, positive_b, _] = table.sample(1.0).to_array();
        assert!(positive_g > positive_r && positive_g > positive_b);

        // Equal magnitudes either side of zero must not collide.
        for magnitude in [0.25_f32, 0.5, 1.0, 2.0] {
            let delta = max_channel_delta(table.sample(-magnitude), table.sample(magnitude));
            assert!(
                delta >= 40,
                "KDP -{magnitude} and +{magnitude} are only {delta} apart"
            );
        }
    }

    #[test]
    fn spectrum_width_offers_more_than_the_one_table_it_used_to() {
        let names = builtin_tables_for_family(ColorTableFamily::SpectrumWidth)
            .into_iter()
            .map(|table| table.name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "Analyst Spectrum Width (interpolated)",
                "Turbulence SW (interpolated)",
                "Clear Air SW (interpolated)",
                "SW Class Bands (stepped)",
            ]
        );

        // The turbulence preset must actually be stretched over 4-12 m/s
        // relative to the default, or it is a different set of colours for no
        // reason.
        let default_band = colour_path(&builtin_spectrum_width_table(), 4.0, 12.0, 40_000);
        let default_whole = colour_path(&builtin_spectrum_width_table(), 0.0, 24.0, 120_000);
        let turbulence_band = colour_path(&turbulence_spectrum_width_table(), 4.0, 12.0, 40_000);
        let turbulence_whole = colour_path(&turbulence_spectrum_width_table(), 0.0, 24.0, 120_000);
        assert!(
            turbulence_band / turbulence_whole > default_band / default_whole + 0.2,
            "turbulence preset {turbulence_band}/{turbulence_whole} vs default {default_band}/{default_whole}"
        );
    }

    /// The dual-pol domains are the Level II field encodings, not round numbers.
    ///
    /// `MomentGrid` decodes a gate as `(raw - offset) / scale`, and the five
    /// cached volumes (KUEX, KABR, KTLX, KLTX, KDMX) all report ZDR with
    /// scale 32 / offset 418 over raw 2..1058, and RHOHV with scale 300 /
    /// offset -60.5 over raw 2..255. Codes 0 and 1 are "below threshold" and
    /// "range folded" and never reach a palette. Writing the constants as those
    /// same quotients makes the endpoints exactly the decoded endpoints, so
    /// nothing a radar can send falls past the last stop and gets flattened.
    #[test]
    fn the_declared_dual_pol_domains_are_the_level_two_field_encodings() {
        assert_eq!((2.0_f32 - 418.0) / 32.0, ZDR_MIN_DB);
        assert_eq!((1058.0_f32 - 418.0) / 32.0, ZDR_MAX_DB);
        assert_eq!((2.0_f32 - -60.5) / 300.0, CC_MIN);
        assert_eq!((255.0_f32 - -60.5) / 300.0, CC_MAX);

        // Nothing clamps: the decoded extremes land on the end stops, not past
        // them, for every table in both families.
        for (family, low, high) in [
            (
                ColorTableFamily::DifferentialReflectivity,
                ZDR_MIN_DB,
                ZDR_MAX_DB,
            ),
            (ColorTableFamily::CorrelationCoefficient, CC_MIN, CC_MAX),
        ] {
            for table in builtin_tables_for_family(family) {
                let first = *table.stops().first().expect("two stops");
                let last = *table.stops().last().expect("two stops");
                assert_eq!(first.value, low, "{} starts late", table.name());
                assert_eq!(last.value, high, "{} stops early", table.name());
                assert_eq!(table.sample(low), first.color);
                assert_eq!(table.sample(high), last.color);
            }
        }
    }

    /// The defect the real volumes exposed, pinned so it cannot come back.
    ///
    /// Both fields pile up hard on their top code: RHOHV code 255 alone holds
    /// 4.1% (KTLX) to 10.3% (KLTX) of all gates while holding 0.012% to 0.037%
    /// of gates above 20 dBZ, and ZDR at or above +8 dB holds 3.4% (KABR) to
    /// 25.2% (KLTX). Both used to be painted in their palette's brightest
    /// colour, so on a coastal or nocturnal scan a quarter of the scope wore
    /// the colour reserved for the most extreme reading - instrument noise
    /// outshining weather. Two invariants stop that: the last stop is never the
    /// brightest stop, and the ceiling sits well below peak brightness.
    #[test]
    fn no_dual_pol_palette_hands_its_brightest_colour_to_the_fields_saturation_code() {
        let brightness = |colour: Rgba8| {
            let [red, green, blue, _] = colour.to_array();
            red as i32 + green as i32 + blue as i32
        };
        let peak_brightness = |table: &ColorTable| {
            table
                .stops()
                .iter()
                .map(|stop| brightness(stop.color))
                .max()
                .expect("two stops")
        };

        for (family, allowed_share) in [
            (ColorTableFamily::DifferentialReflectivity, 0.40_f64),
            (ColorTableFamily::CorrelationCoefficient, 0.70),
        ] {
            for table in builtin_tables_for_family(family) {
                let peak = peak_brightness(&table);
                let last = brightness(table.stops().last().expect("two stops").color);
                assert!(
                    last < peak,
                    "{} makes its last stop the brightest colour it owns",
                    table.name()
                );
                let share = last as f64 / peak as f64;
                assert!(
                    share <= allowed_share,
                    "{} paints the saturation code at {share:.2} of peak brightness",
                    table.name()
                );
            }
        }

        // The negative ZDR end is a noise floor too, and gets the same rule.
        for table in builtin_tables_for_family(ColorTableFamily::DifferentialReflectivity) {
            let peak = peak_brightness(&table);
            let first = brightness(table.stops().first().expect("two stops").color);
            assert!(
                (first as f64) < 0.40 * peak as f64,
                "{} lights up ZDR below -13 dB",
                table.name()
            );
        }
    }

    /// Widening ZDR to the field's own range added stops outside -7 to +8 dB
    /// and moved none inside it, so every gate that was already on scale
    /// samples the identical colour. Probed at stop values, whose expected
    /// colours can be read straight off the lists above.
    #[test]
    fn widening_the_zdr_domain_left_the_meteorological_scale_untouched() {
        let analyst = analyst_differential_reflectivity_table();
        assert_eq!(analyst.sample(ZDR_MET_MIN_DB), Rgba8::opaque(58, 10, 92));
        assert_eq!(analyst.sample(-1.0), Rgba8::opaque(96, 122, 208));
        assert_eq!(analyst.sample(2.0), Rgba8::opaque(198, 224, 64));
        assert_eq!(analyst.sample(4.0), Rgba8::opaque(238, 62, 44));
        assert_eq!(analyst.sample(7.0), Rgba8::opaque(228, 152, 228));
        assert_eq!(analyst.sample(ZDR_MET_MAX_DB), Rgba8::opaque(246, 246, 250));

        // Stepped: a probe inside a band paints that band's colour, and the
        // band that starts at +8 dB is the off-scale one.
        let storm = storm_interrogation_differential_reflectivity_table();
        assert_eq!(storm.sample(ZDR_MET_MIN_DB), Rgba8::opaque(72, 20, 110));
        assert_eq!(storm.sample(-0.75), Rgba8::opaque(86, 106, 178));
        assert_eq!(storm.sample(0.0), Rgba8::opaque(112, 112, 116));
        assert_eq!(storm.sample(2.5), Rgba8::opaque(206, 222, 62));
        assert_eq!(storm.sample(7.0), Rgba8::opaque(200, 48, 176));
        assert_eq!(storm.sample(ZDR_MET_MAX_DB), Rgba8::opaque(56, 124, 130));

        let column = zdr_column_hunter_table();
        assert_eq!(column.sample(ZDR_MET_MIN_DB), Rgba8::opaque(10, 10, 16));
        assert_eq!(column.sample(0.5), Rgba8::opaque(14, 18, 30));
        assert_eq!(column.sample(3.0), Rgba8::opaque(232, 216, 62));
        assert_eq!(column.sample(5.0), Rgba8::opaque(232, 108, 200));

        let hail = hail_signal_differential_reflectivity_table();
        assert_eq!(hail.sample(ZDR_MET_MIN_DB), Rgba8::opaque(28, 6, 48));
        assert_eq!(hail.sample(0.0), Rgba8::opaque(250, 250, 250));
        assert_eq!(hail.sample(4.0), Rgba8::opaque(56, 60, 96));
        assert_eq!(hail.sample(ZDR_MET_MAX_DB), Rgba8::opaque(24, 30, 52));
    }

    /// The pairs a forecaster actually has to tell apart, on the family
    /// defaults. Expected colours read off the stop lists: ZDR 0.5 dB is a
    /// third of the way from (24,96,62) at 0.4 dB to (26,140,74) at 0.7 dB,
    /// giving (25,111,66) against the near-zero plateau's (157,157,161).
    #[test]
    fn the_default_dual_pol_tables_separate_the_readings_that_carry_meaning() {
        let zdr = builtin_differential_reflectivity_table();
        assert_eq!(zdr.sample(0.0), Rgba8::opaque(157, 157, 161));
        assert_eq!(zdr.sample(0.5), Rgba8::opaque(25, 111, 66));
        assert_eq!(max_channel_delta(zdr.sample(0.0), zdr.sample(0.5)), 132);
        assert_eq!(zdr.sample(3.0), Rgba8::opaque(248, 170, 44));
        assert_eq!(zdr.sample(5.0), Rgba8::opaque(208, 30, 98));
        assert_eq!(max_channel_delta(zdr.sample(3.0), zdr.sample(5.0)), 140);

        let cc = builtin_correlation_coefficient_table();
        assert_eq!(cc.sample(0.95), Rgba8::opaque(120, 206, 84));
        assert_eq!(cc.sample(0.99), Rgba8::opaque(56, 84, 216));
        assert_eq!(max_channel_delta(cc.sample(0.95), cc.sample(0.99)), 132);
        assert_eq!(cc.sample(0.80), Rgba8::opaque(226, 86, 48));
        assert_eq!(cc.sample(0.90), Rgba8::opaque(246, 196, 52));
        assert_eq!(max_channel_delta(cc.sample(0.80), cc.sample(0.90)), 110);
    }

    /// 0.80 is the debris threshold and 0.90 the top of the mixed-hydrometeor
    /// band (Ryzhkov et al. 2005; Kumjian 2013 Part I), so every preset has to
    /// separate them, not just the default.
    #[test]
    fn every_correlation_coefficient_table_separates_the_debris_threshold_from_point_nine() {
        for table in builtin_tables_for_family(ColorTableFamily::CorrelationCoefficient) {
            let delta = max_channel_delta(table.sample(0.80), table.sample(0.90));
            assert!(
                delta >= 40,
                "{} renders CC 0.80 and 0.90 only {delta} apart",
                table.name()
            );
        }
    }

    /// PHIDP folds at the top of its encoding, not at a round 360. The word
    /// carries scale 2.8361 and offset 2 over raw 2..1022, so the last value
    /// before the fold is (1022 - 2)/2.8361 = 359.65 deg and the next gate
    /// along the ray reads 0. That is the step that must not draw an edge, and
    /// the cached volumes contain thousands of rays that take it.
    #[test]
    fn the_phase_tables_close_at_the_fold_the_encoding_actually_produces() {
        const LAST_BEFORE_FOLD: f32 = 359.6488;

        let analyst = analyst_differential_phase_table();
        assert_eq!(analyst.sample(LAST_BEFORE_FOLD), Rgba8::opaque(242, 36, 37));
        assert_eq!(analyst.sample(0.0), Rgba8::opaque(242, 36, 36));

        let twilight = twilight_cyclic_differential_phase_table();
        assert_eq!(
            twilight.sample(LAST_BEFORE_FOLD),
            Rgba8::opaque(226, 216, 225)
        );
        assert_eq!(twilight.sample(0.0), Rgba8::opaque(226, 217, 226));

        for table in [analyst, twilight] {
            assert!(
                max_channel_delta(table.sample(LAST_BEFORE_FOLD), table.sample(0.0)) <= 3,
                "{} draws an edge at the real fold",
                table.name()
            );
        }

        // The stepped preset steps, but by exactly one band, the same step it
        // takes at every other boundary.
        let bands = phase_bands_differential_phase_table();
        assert_eq!(bands.sample(LAST_BEFORE_FOLD), Rgba8::opaque(242, 36, 88));
        assert_eq!(bands.sample(0.0), Rgba8::opaque(242, 36, 36));
        assert_eq!(
            max_channel_delta(bands.sample(LAST_BEFORE_FOLD), bands.sample(0.0)),
            max_channel_delta(bands.sample(14.0), bands.sample(16.0))
        );
    }

    /// A stepped table must not collapse two adjacent bands onto one colour,
    /// and an interpolated one must not stall. Checked across every new table
    /// by walking its own stop list.
    #[test]
    fn no_new_table_paints_two_adjacent_stops_the_same_colour() {
        for family in [
            ColorTableFamily::SpectrumWidth,
            ColorTableFamily::DifferentialReflectivity,
            ColorTableFamily::CorrelationCoefficient,
            ColorTableFamily::DifferentialPhase,
            ColorTableFamily::SpecificDifferentialPhase,
        ] {
            for table in builtin_tables_for_family(family) {
                for window in table.stops().windows(2) {
                    let (left, right) = (window[0], window[1]);
                    // The one deliberate exception: Hail Signal ZDR repeats a
                    // colour to hold a flat plateau across the near-zero band,
                    // which is the signature being drawn.
                    if left.color == right.color && table.name().starts_with("Hail Signal ZDR") {
                        continue;
                    }
                    assert!(
                        max_channel_delta(left.color, right.color) >= 3,
                        "{} paints {} and {} the same colour",
                        table.name(),
                        left.value,
                        right.value
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Nothing moved
    //
    // Adding interpolated presets and renaming every built-in is only useful if
    // the tables that were already on the scope still paint exactly what they
    // painted. Otherwise an analyst comparing a stepped table against a smooth
    // one is comparing two things that both changed, and can conclude nothing
    // about either.
    //
    // Every expectation below is read off the palette text by hand. For a
    // quantized table that means: quantise the probe onto the table's own step
    // grid first (`quantize_value`, round-half-away-from-zero), then look the
    // quantised value up, interpolating between the two stops that bracket it.
    // -----------------------------------------------------------------------

    /// The default reflectivity table, probed at nine values.
    ///
    /// GR2Analyst Classic REF declares `step: 5`, so `sample` rounds the gate's
    /// dBZ to the nearest multiple of 5 before looking it up. Six of these
    /// probes land on a declared stop and return it unchanged; three land
    /// between stops and are interpolated, and those three are written out
    /// longhand because they are where an arithmetic change would show first.
    #[test]
    fn the_default_reflectivity_table_still_paints_exactly_what_it_did() {
        let table = builtin_reflectivity_table();

        assert_eq!(table.step_size(), Some(5.0));
        assert_eq!(table.sample_mode_label(), "quantized stepped");

        // Below the first inked stop: nothing, at any distance.
        assert_eq!(table.sample(9.999), Rgba8::TRANSPARENT);
        assert_eq!(table.sample(-30.0), Rgba8::TRANSPARENT);

        // Declared stops, returned unchanged.
        assert_eq!(table.sample(10.0), Rgba8::opaque(4, 233, 231));
        assert_eq!(table.sample(20.0), Rgba8::opaque(3, 0, 244));
        assert_eq!(table.sample(30.0), Rgba8::opaque(1, 197, 1));
        assert_eq!(table.sample(35.0), Rgba8::opaque(0, 142, 0));
        assert_eq!(table.sample(50.0), Rgba8::opaque(253, 149, 0));

        // The 5 dBZ grid, either side of the 37.5 dBZ midpoint: 37.4 rounds
        // down onto 35 and 37.6 rounds up onto 40.
        assert_eq!(table.sample(37.4), Rgba8::opaque(0, 142, 0));
        assert_eq!(table.sample(37.6), Rgba8::opaque(253, 248, 2));

        // Interpolated between stops. 60 dBZ is two thirds of the way from
        // (253,0,0) at 55 to (212,0,0) at 62.5: 253 - 41 * 2/3 = 225.67 -> 226.
        assert_eq!(table.sample(60.0), Rgba8::opaque(226, 0, 0));
        // 65 is halfway from (212,0,0) at 62.5 to (188,0,0) at 67.5.
        assert_eq!(table.sample(65.0), Rgba8::opaque(200, 0, 0));
        // 70 is halfway from (188,0,0) at 67.5 to (232,32,206) at 72.5:
        // 188+22, 0+16, 0+103.
        assert_eq!(table.sample(70.0), Rgba8::opaque(210, 16, 103));
        // 75 is a third of the way from (232,32,206) at 72.5 to (156,70,206)
        // at 80: 232-25.33 -> 207, 32+12.67 -> 45, blue unchanged.
        assert_eq!(table.sample(75.0), Rgba8::opaque(207, 45, 206));
    }

    /// The default velocity table, probed at ten values.
    ///
    /// Analyst Tornado VEL declares `step: 2` and `units: m/s`, so no unit
    /// rescaling happens and the probe is rounded onto even m/s.
    #[test]
    fn the_default_velocity_table_still_paints_exactly_what_it_did() {
        let table = builtin_velocity_table();

        assert_eq!(table.step_size(), Some(2.0));
        assert_eq!(table.sample_mode_label(), "quantized stepped");
        // No RF row in the palette, so the module default stands.
        assert_eq!(table.range_folded_rgba(), Rgba8::new(126, 80, 196, 245));

        // Declared stops.
        assert_eq!(table.sample(-30.0), Rgba8::opaque(246, 255, 255));
        assert_eq!(table.sample(-18.0), Rgba8::opaque(0, 156, 54));
        assert_eq!(table.sample(-2.0), Rgba8::opaque(84, 100, 84));
        assert_eq!(table.sample(0.0), Rgba8::opaque(112, 112, 112));
        assert_eq!(table.sample(2.0), Rgba8::opaque(120, 86, 84));
        assert_eq!(table.sample(20.0), Rgba8::opaque(242, 0, 0));
        assert_eq!(table.sample(34.0), Rgba8::opaque(255, 224, 168));
        assert_eq!(table.sample(50.0), Rgba8::opaque(255, 255, 240));

        // Interpolated. -12 is a quarter of the way from (18,232,54) at -13 to
        // (82,244,104) at -9: 18+16, 232+3, 54+12.5 -> 67.
        assert_eq!(table.sample(-12.0), Rgba8::opaque(34, 235, 67));
        // +10 is a fifth of the way from (216,28,28) at 9 to (255,34,40) at 14:
        // 216+7.8 -> 224, 28+1.2 -> 29, 28+2.4 -> 30.
        assert_eq!(table.sample(10.0), Rgba8::opaque(224, 29, 30));
    }

    /// One probe on every other registered reflectivity and velocity preset.
    ///
    /// Each probe is chosen to land on a declared stop after quantisation, so
    /// the expected colour is the palette text's own triple with no arithmetic
    /// in between. Cheap, and it catches a whole table being swapped, rescaled,
    /// or re-parsed into a different mode.
    #[test]
    fn every_other_registered_stepped_preset_still_paints_exactly_what_it_did() {
        for (table, probe, expected) in [
            (
                analyst_classic_reflectivity_table(),
                25.0,
                Rgba8::opaque(0, 222, 44),
            ),
            (nws_reflectivity_table(), 45.0, Rgba8::opaque(229, 188, 0)),
            (
                dark_scope_reflectivity_table(),
                40.0,
                Rgba8::opaque(232, 156, 42),
            ),
            (
                hail_core_reflectivity_table(),
                50.0,
                Rgba8::opaque(246, 26, 28),
            ),
            (
                low_precip_reflectivity_table(),
                35.0,
                Rgba8::opaque(224, 226, 64),
            ),
            (
                tornado_debris_reflectivity_table(),
                30.0,
                Rgba8::opaque(72, 176, 42),
            ),
            (
                clean_light_reflectivity_table(),
                37.5,
                Rgba8::opaque(220, 218, 58),
            ),
            (analyst_velocity_table(), -15.0, Rgba8::opaque(0, 226, 58)),
            (
                radarscope_contrast_velocity_table(),
                16.0,
                Rgba8::opaque(255, 40, 46),
            ),
            (
                couplet_pop_velocity_table(),
                -10.0,
                Rgba8::opaque(34, 186, 48),
            ),
            (
                gr2_ish_analyst_velocity_table(),
                24.0,
                Rgba8::opaque(246, 0, 0),
            ),
            (
                subtle_srv_velocity_table(),
                16.0,
                Rgba8::opaque(222, 64, 58),
            ),
        ] {
            assert_eq!(
                table.sample(probe),
                expected,
                "{} moved at {probe}",
                table.name()
            );
        }
    }

    /// The rename is a rename: the same palette, wearing its mode.
    #[test]
    fn renaming_the_built_ins_did_not_touch_their_stops() {
        // Read off GR2_REFLECTIVITY_TABLE: 17 rows, the first two transparent.
        let table = builtin_reflectivity_table();
        assert_eq!(table.stops().len(), 17);
        assert_eq!(table.stops()[0].value, -10.0);
        assert_eq!(table.stops()[0].color, Rgba8::TRANSPARENT);
        assert_eq!(table.stops()[1].value, 7.5);
        assert_eq!(table.stops()[1].color, Rgba8::TRANSPARENT);
        assert_eq!(table.stops()[2].color, Rgba8::opaque(4, 233, 231));
        assert_eq!(table.product(), Some("BR"));
        assert_eq!(table.units(), Some("dBZ"));
        assert_eq!(table.inked_value_span(), Some((10.0, 92.5)));

        // And the name is the old name plus the mode, nothing else.
        assert_eq!(
            table.name(),
            format!("GR2Analyst Classic REF ({})", table.sample_mode_label())
        );
    }

    // -----------------------------------------------------------------------
    // The new interpolated presets
    // -----------------------------------------------------------------------

    /// The four dBZ values a reflectivity palette has to keep legible.
    const REFLECTIVITY_BREAKS: [f32; 4] = [20.0, 35.0, 50.0, 65.0];

    /// The name is the only string a picker row shows, so it has to carry the
    /// one fact that decides what the table will look like on the scope.
    #[test]
    fn every_builtin_tables_name_ends_with_its_own_sampling_mode() {
        for (label, table, _) in every_builtin_table_with_expected_span() {
            let suffix = format!(" ({})", table.sample_mode_label());
            assert!(
                table.name().ends_with(&suffix),
                "{label} is named {:?}, which does not end with {suffix:?}",
                table.name()
            );
            // Not just a suffix: something has to come before it.
            assert!(
                table.name().len() > suffix.len(),
                "{label} is nothing but its mode"
            );
        }

        // The three wordings, spelled the way sample_mode_label spells them.
        assert_eq!(
            builtin_reflectivity_table().name(),
            "GR2Analyst Classic REF (quantized stepped)"
        );
        assert_eq!(
            smooth_classic_reflectivity_table().name(),
            "Smooth Classic REF (interpolated)"
        );
        assert_eq!(
            sign_check_velocity_table().name(),
            "Sign Check VEL (stepped)"
        );
    }

    /// Sorted, finite, and never silently collapsed.
    ///
    /// `from_parts` sorts and de-duplicates, so this cannot fail by accident -
    /// which is the problem. A palette that declares two stops at the same
    /// value loses one of them without a word, and the table still validates.
    /// Checking the count as well as the ordering is what turns that into a
    /// failure: `no_new_interpolated_table_lost_a_stop_to_de_duplication`
    /// carries the counts for the tables written as stop lists here.
    #[test]
    fn every_builtin_tables_stops_are_finite_and_strictly_increasing() {
        for (label, table, _) in every_builtin_table_with_expected_span() {
            assert!(table.stops().len() >= 2, "{label} has under two stops");
            for stop in table.stops() {
                assert!(stop.value.is_finite(), "{label} carries a non-finite stop");
            }
            for window in table.stops().windows(2) {
                assert!(
                    window[0].value < window[1].value,
                    "{label} is out of order or duplicated at {}",
                    window[0].value
                );
            }
            // Transparency only ever leads. An interior alpha-0 stop would
            // punch a hole in the middle of the scale and inked_value_span
            // would report straight across it.
            let clear_stops = table
                .stops()
                .iter()
                .take_while(|stop| stop.color.a == 0)
                .count();
            assert!(
                table.stops()[clear_stops..]
                    .iter()
                    .all(|stop| stop.color.a > 0),
                "{label} has a transparent stop after its first inked one"
            );
        }
    }

    /// Hand-counted off the stop lists, so a duplicated value shows up as a
    /// missing stop instead of vanishing into `from_parts`.
    #[test]
    fn no_new_interpolated_table_lost_a_stop_to_de_duplication() {
        for (table, expected) in [
            (smooth_classic_reflectivity_table(), 36),
            (smooth_sequential_reflectivity_table(), 34),
            (smooth_storm_core_reflectivity_table(), 35),
            (smooth_doppler_velocity_table(), 33),
            (smooth_couplet_velocity_table(), 31),
        ] {
            assert_eq!(
                table.stops().len(),
                expected,
                "{} declared {expected} stops",
                table.name()
            );
            assert!(table.interpolates(), "{} must interpolate", table.name());
            assert_eq!(
                table.step_size(),
                None,
                "{} must not quantize",
                table.name()
            );
        }
    }

    /// The point of the whole exercise: a smooth table resolves the field, a
    /// stepped one bins it.
    ///
    /// Swept at 0.5 dBZ, which is the resolution Level II reflectivity is
    /// encoded at (scale 2, offset 66), so this is every distinct value a gate
    /// between 10 and 70 dBZ can carry - 121 of them.
    ///
    /// The stepped default can answer with 13 colours and no more, and they are
    /// hand-enumerable: it rounds onto multiples of 5, so 10, 15, 20 ... 70,
    /// thirteen quantisation levels, each a different colour in the palette
    /// text. Every one of the 121 values collapses onto one of those thirteen.
    ///
    /// The interpolated tables have no such ceiling. They can only lose a value
    /// where their ramp moves less than one 8-bit unit in any channel across
    /// 0.5 dBZ, which happens only in their flattest stretches.
    #[test]
    fn an_interpolated_table_resolves_the_reflectivity_scale_a_stepped_one_bins() {
        let distinct_colours = |table: &ColorTable| {
            let mut seen = std::collections::HashSet::new();
            // 10.0 to 70.0 inclusive in exact halves: 0.5 is a power of two, so
            // the accumulation is exact in f32 and lands on 121 distinct values.
            for step in 0..=120 {
                let value = 10.0 + step as f32 * 0.5;
                seen.insert(table.sample(value));
            }
            seen.len()
        };

        let stepped = distinct_colours(&builtin_reflectivity_table());
        assert_eq!(stepped, 13, "the 5 dBZ grid from 10 to 70 has 13 levels");

        for table in [
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
        ] {
            let smooth = distinct_colours(&table);
            assert!(
                smooth >= 110,
                "{} resolves only {smooth} of the 121 values in 10-70 dBZ",
                table.name()
            );
            assert!(
                smooth >= 8 * stepped,
                "{} resolves {smooth} colours against the stepped default's {stepped}",
                table.name()
            );
        }
    }

    /// A gradient that hides the 50 dBZ core is prettier and worse.
    ///
    /// All three interpolated reflectivity presets answer that by turning
    /// inside a single 0.5 dBZ window - one Level II step, so the turn is drawn
    /// as a contour a gate wide - at each of the four break points, and gliding
    /// everywhere else. Measured as the largest per-channel change across each
    /// half-dBZ window from 10 to 75 dBZ: the four windows ending on a break
    /// must each move at least two and a half times as far as the worst window
    /// that does not.
    ///
    /// Smooth Sequential REF used to be exempt from this test, and it failed
    /// it: it moved 3-8 units across the four break windows against 8 units for
    /// the worst ordinary window, so no operational threshold was visible on
    /// it. The exemption was the bug. Its breaks are steps up in luminance
    /// rather than hue turns, which keeps the table monotone in lightness and
    /// still passes here, so it is now held to the same bar as the other two.
    #[test]
    fn the_contour_tables_turn_at_the_four_breaks_and_glide_between_them() {
        for table in [
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
        ] {
            let mut break_steps = Vec::new();
            let mut worst_glide = 0;
            let mut low = 10.0_f32;
            while low < 75.0 {
                let high = low + 0.5;
                let step = max_channel_delta(table.sample(low), table.sample(high));
                if REFLECTIVITY_BREAKS.contains(&high) {
                    break_steps.push((high, step));
                } else {
                    worst_glide = worst_glide.max(step);
                }
                low = high;
            }

            assert_eq!(break_steps.len(), 4, "{} lost a break", table.name());
            for (value, step) in break_steps {
                assert!(
                    step as f64 >= 2.5 * worst_glide as f64,
                    "{} moves {step} across the {value} dBZ break but up to \
                     {worst_glide} elsewhere, so the break is not readable",
                    table.name()
                );
            }
            // And the glide has to be a glide: a table that only moved at the
            // breaks would pass the check above and be a four-band palette.
            assert!(
                worst_glide >= 4,
                "{} barely moves between breaks",
                table.name()
            );
        }
    }

    /// Smooth Sequential REF is the diagnostic table, and its whole claim is
    /// that lightness never doubles back.
    ///
    /// BT.709 relative luminance, the standard sRGB weighting. Checked stop by
    /// stop rather than by sweeping, because between two stops the channels
    /// move linearly and so does any weighted sum of them: if luminance rises
    /// from each stop to the next it rises everywhere. (Strictly, the *rendered*
    /// 8-bit colour can dip by up to a quarter of a luminance unit inside a stop
    /// interval, because `lerp_u8` rounds each channel independently. That is
    /// quantisation noise in the output word, not a fold in the ramp, and it is
    /// three orders of magnitude below the 240 units the ramp climbs.)
    #[test]
    fn the_sequential_reflectivity_table_never_darkens_as_reflectivity_rises() {
        let table = smooth_sequential_reflectivity_table();
        let luminance = |colour: Rgba8| {
            let [red, green, blue, _] = colour.to_array();
            0.2126 * red as f64 + 0.7152 * green as f64 + 0.0722 * blue as f64
        };

        let inked: Vec<_> = table
            .stops()
            .iter()
            .filter(|stop| stop.color.a > 0)
            .collect();
        assert_eq!(inked.len(), 32);
        for window in inked.windows(2) {
            let (dark, light) = (luminance(window[0].color), luminance(window[1].color));
            assert!(
                light > dark,
                "{} darkens from {} to {} dBZ ({dark:.1} -> {light:.1})",
                table.name(),
                window[0].value,
                window[1].value
            );
        }

        // Ends dark-to-bright over the full inked span, not just locally.
        assert!(luminance(table.sample(95.0)) - luminance(table.sample(10.0)) > 200.0);

        // Each break is readable against the gates either side of it, which is
        // the check the first version of this test got wrong: it compared the
        // four breaks to EACH OTHER - 15 dBZ apart, so of course they differed -
        // and never asked whether 49.5 dBZ could be told from 50.0. It could
        // not. An analyst reads a threshold by seeing the gates on one side of
        // it change colour, so the comparison has to be against the neighbours.
        for anchor in REFLECTIVITY_BREAKS {
            let across = max_channel_delta(table.sample(anchor - 0.5), table.sample(anchor));
            let below = max_channel_delta(table.sample(anchor - 1.0), table.sample(anchor - 0.5));
            let above = max_channel_delta(table.sample(anchor), table.sample(anchor + 0.5));
            assert!(
                across >= 5 * below.max(above).max(1),
                "{} moves {across} across the {anchor} dBZ break but {below} and \
                 {above} in the half-dBZ windows either side, so the threshold \
                 is not visible on the scope",
                table.name()
            );
        }
    }

    /// Smooth Storm Core REF exists to spend its colour on 35-65 dBZ. If it
    /// does not, it is Smooth Classic REF with different numbers.
    #[test]
    fn the_storm_core_table_spends_its_colour_on_the_convective_range() {
        let core = smooth_storm_core_reflectivity_table();
        let classic = smooth_classic_reflectivity_table();

        let share = |table: &ColorTable| {
            colour_path(table, 35.0, 65.0, 60_000) / colour_path(table, 10.0, 95.0, 170_000)
        };
        let core_share = share(&core);
        let classic_share = share(&classic);

        assert!(
            core_share > 0.55,
            "storm core spends only {core_share:.2} of its colour on 35-65 dBZ"
        );
        assert!(
            core_share > classic_share + 0.1,
            "storm core {core_share:.2} is no more concentrated than classic {classic_share:.2}"
        );

        // Below 35 dBZ it stays desaturated, so stratiform rain is locatable
        // without competing with the core for attention. Chroma here is the
        // crude max-minus-min channel spread, which is enough to separate a
        // slate blue from a saturated hue and can be read off the stop list.
        let chroma = |value: f32| {
            let [red, green, blue, _] = core.sample(value).to_array();
            red.max(green).max(blue) as i32 - red.min(green).min(blue) as i32
        };
        let core_band = [45.0_f32, 50.0, 55.0]
            .into_iter()
            .map(chroma)
            .min()
            .expect("three probes");
        for value in [12.5_f32, 20.0, 27.5, 34.0] {
            assert!(
                chroma(value) * 2 < core_band,
                "{value} dBZ carries chroma {} against the core band's {core_band}",
                chroma(value)
            );
        }
    }

    /// The three interpolated reflectivity presets have to paint the same gates
    /// as the stepped ones, or switching between them changes the echo's shape
    /// and the comparison they exist for is worthless.
    ///
    /// On every value a Level II reflectivity word can hold they do: the field
    /// decodes as (raw - 66) / 2, so it only ever lands on the 0.5 dBZ grid, and
    /// both 9.5 and 10.0 are grid points.
    ///
    /// Off that grid they do not, and the second half of this test pins the
    /// difference rather than pretending it away. `render2d`'s Soften and
    /// Interpolate display passes produce smoothed physical values between grid
    /// points - one to two percent of gates land in 9.5 < dBZ < 10.0 on real
    /// volumes - and there an interpolated table is part-way along its alpha
    /// ramp while a quantised one is still fully clear, because
    /// `SampleMode::QuantizedInterpolated` short-circuits to transparent below
    /// its first opaque stop. The visible consequence is one half-dBZ of extra
    /// softness at the outer edge of the echo, and nothing at all inside it.
    #[test]
    fn the_interpolated_reflectivity_presets_ink_the_same_gates_as_the_stepped_ones() {
        for table in [
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
        ] {
            assert_eq!(
                table.inked_value_span(),
                Some((10.0, 95.0)),
                "{} does not ink from 10 dBZ",
                table.name()
            );
            assert_eq!(table.sample(5.0), Rgba8::TRANSPARENT);
            assert_eq!(table.sample(9.5), Rgba8::TRANSPARENT);
            assert_eq!(table.sample(10.0).a, 255);
            // The one half-painted step, and it is the only one: 9.75 sits
            // halfway across the 0.5 dBZ alpha ramp.
            assert_eq!(table.sample(9.75).a, 128);

            // Every value the encoding can actually produce agrees with the
            // stepped default, gate for gate, in both directions.
            let stepped = builtin_reflectivity_table();
            for raw in 0_u16..=255 {
                let dbz = (raw as f32 - 66.0) / 2.0;
                assert_eq!(
                    table.sample(dbz).a > 0,
                    stepped.sample(dbz).a > 0,
                    "{} and {} disagree about whether {dbz} dBZ is painted",
                    table.name(),
                    stepped.name()
                );
            }
        }

        // Hand-read off Smooth Classic REF: at 9.75 the colour is halfway from
        // transparent black to (16,88,140), so every channel halves too.
        assert_eq!(
            smooth_classic_reflectivity_table().sample(9.75),
            Rgba8::new(8, 44, 70, 128)
        );
        // And the stepped default at the same off-grid value is fully clear,
        // which is the whole of the difference between the two.
        assert_eq!(
            builtin_reflectivity_table().sample(9.75),
            Rgba8::TRANSPARENT
        );
    }

    /// Hand-read off the three stop lists. Break anchors first, then one
    /// interpolated point in each, which is where an arithmetic change shows.
    #[test]
    fn the_interpolated_reflectivity_presets_paint_their_declared_colours() {
        let classic = smooth_classic_reflectivity_table();
        assert_eq!(classic.sample(19.5), Rgba8::opaque(40, 208, 244));
        assert_eq!(classic.sample(20.0), Rgba8::opaque(14, 148, 60));
        assert_eq!(classic.sample(35.0), Rgba8::opaque(250, 228, 36));
        assert_eq!(classic.sample(50.0), Rgba8::opaque(230, 18, 26));
        assert_eq!(classic.sample(65.0), Rgba8::opaque(214, 40, 200));
        // Halfway from (16,88,140) at 10 to (18,118,176) at 12.5.
        assert_eq!(classic.sample(11.25), Rgba8::opaque(17, 103, 158));
        // Halfway from (14,148,60) at 20 to (16,168,58) at 22.5.
        assert_eq!(classic.sample(21.25), Rgba8::opaque(15, 158, 59));

        let sequential = smooth_sequential_reflectivity_table();
        assert_eq!(sequential.sample(19.5), Rgba8::opaque(50, 21, 116));
        assert_eq!(sequential.sample(20.0), Rgba8::opaque(98, 26, 144));
        assert_eq!(sequential.sample(34.5), Rgba8::opaque(168, 55, 145));
        assert_eq!(sequential.sample(35.0), Rgba8::opaque(226, 66, 66));
        assert_eq!(sequential.sample(49.5), Rgba8::opaque(252, 122, 32));
        assert_eq!(sequential.sample(50.0), Rgba8::opaque(255, 178, 26));
        assert_eq!(sequential.sample(64.5), Rgba8::opaque(253, 210, 82));
        assert_eq!(sequential.sample(65.0), Rgba8::opaque(248, 248, 176));
        // Halfway from (255,178,26) at 50 to (255,184,34) at 52.5: green
        // 178 + 6/2 = 181, blue 26 + 8/2 = 30.
        assert_eq!(sequential.sample(51.25), Rgba8::opaque(255, 181, 30));

        let core = smooth_storm_core_reflectivity_table();
        assert_eq!(core.sample(34.5), Rgba8::opaque(74, 104, 152));
        assert_eq!(core.sample(35.0), Rgba8::opaque(24, 152, 78));
        assert_eq!(core.sample(49.5), Rgba8::opaque(250, 178, 40));
        assert_eq!(core.sample(50.0), Rgba8::opaque(248, 80, 30));
        // Halfway from (24,152,78) at 35 to (46,182,66) at 37.5.
        assert_eq!(core.sample(36.25), Rgba8::opaque(35, 167, 72));
    }

    /// The two interpolated velocity presets, held to the conventions that make
    /// a velocity display readable at all.
    #[test]
    fn the_interpolated_velocity_presets_keep_zero_neutral_and_sign_legible() {
        for table in [
            smooth_doppler_velocity_table(),
            smooth_couplet_velocity_table(),
        ] {
            // Zero is grey, so the zero isodop reads as a line and not a colour.
            let [zero_r, zero_g, zero_b, zero_a] = table.sample(0.0).to_array();
            assert_eq!(zero_a, 255);
            assert_eq!(zero_r, zero_g, "{} zero is not neutral", table.name());
            assert_eq!(zero_g, zero_b, "{} zero is not neutral", table.name());

            // Inbound cool - green low, cyan high - and outbound warm, at every
            // magnitude an analyst reads. Stated as "red is the weakest channel
            // inbound and the strongest outbound" because the inbound half
            // deliberately crosses from green to cyan at its strong end, the
            // way every stepped velocity preset in this module does.
            for magnitude in [5.0_f32, 10.0, 15.0, 20.0, 25.0, 30.0] {
                let [in_r, in_g, in_b, _] = table.sample(-magnitude).to_array();
                assert!(
                    in_g > in_r && in_b > in_r,
                    "{} paints -{magnitude} m/s {in_r},{in_g},{in_b}, which is not inbound",
                    table.name()
                );
                let [out_r, out_g, out_b, _] = table.sample(magnitude).to_array();
                assert!(
                    out_r > out_g && out_r > out_b,
                    "{} paints +{magnitude} m/s {out_r},{out_g},{out_b}, which is not outbound",
                    table.name()
                );
                // And the two signs must never collide, which is the failure
                // that makes a couplet disappear.
                assert!(
                    max_channel_delta(table.sample(-magnitude), table.sample(magnitude)) >= 60,
                    "{} renders -{magnitude} and +{magnitude} m/s alike",
                    table.name()
                );
            }

            assert_eq!(table.inked_value_span(), Some((-70.0, 70.0)));
        }
    }

    /// Smooth Couplet VEL exists to spend its colour inside +/-25 m/s, where
    /// mesocyclonic and tornadic couplets sit and where most base velocity data
    /// lives anyway. If it is no more concentrated than Smooth Doppler VEL it
    /// is a second copy of it.
    #[test]
    fn the_couplet_velocity_table_concentrates_on_the_rotational_band() {
        let couplet = smooth_couplet_velocity_table();
        let doppler = smooth_doppler_velocity_table();

        let share = |table: &ColorTable| {
            colour_path(table, -25.0, 25.0, 50_000) / colour_path(table, -70.0, 70.0, 140_000)
        };
        let couplet_share = share(&couplet);
        let doppler_share = share(&doppler);

        // +/-25 m/s is 50 of the 140 m/s domain, or 35.7%.
        assert!((50.0 / 140.0_f64 - 0.357).abs() < 0.001);
        assert!(
            couplet_share > 0.55,
            "couplet table spends only {couplet_share:.2} of its colour inside +/-25 m/s"
        );
        assert!(
            couplet_share > doppler_share + 0.05,
            "couplet {couplet_share:.2} is no more concentrated than doppler {doppler_share:.2}"
        );
    }

    /// Hand-read off the two velocity stop lists.
    #[test]
    fn the_interpolated_velocity_presets_paint_their_declared_colours() {
        let doppler = smooth_doppler_velocity_table();
        assert_eq!(doppler.sample(0.0), Rgba8::opaque(112, 112, 112));
        assert_eq!(doppler.sample(-20.0), Rgba8::opaque(22, 228, 62));
        assert_eq!(doppler.sample(20.0), Rgba8::opaque(250, 26, 26));
        // Halfway from (18,174,54) at -12 to (16,146,50) at -8.
        assert_eq!(doppler.sample(-10.0), Rgba8::opaque(17, 160, 52));
        // Halfway from (190,40,42) at 8 to (212,28,32) at 12.
        assert_eq!(doppler.sample(10.0), Rgba8::opaque(201, 34, 37));

        let couplet = smooth_couplet_velocity_table();
        assert_eq!(couplet.sample(0.0), Rgba8::opaque(104, 104, 104));
        assert_eq!(couplet.sample(-25.0), Rgba8::opaque(0, 206, 214));
        assert_eq!(couplet.sample(-15.0), Rgba8::opaque(18, 232, 70));
        assert_eq!(couplet.sample(15.0), Rgba8::opaque(255, 44, 28));
        assert_eq!(couplet.sample(25.0), Rgba8::opaque(255, 188, 28));
        // Halfway from (14,186,56) at -9 to (26,158,56) at -6.
        assert_eq!(couplet.sample(-7.5), Rgba8::opaque(20, 172, 56));
    }

    /// The interpolated presets get the same adjacent-stop check the dual-pol
    /// tables get: a repeated colour is a stop that does nothing.
    #[test]
    fn no_interpolated_preset_paints_two_adjacent_stops_the_same_colour() {
        for table in [
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
            smooth_doppler_velocity_table(),
            smooth_couplet_velocity_table(),
        ] {
            for window in table.stops().windows(2) {
                let (left, right) = (window[0], window[1]);
                // The two leading clear stops are both transparent by design.
                if left.color.a == 0 && right.color.a == 0 {
                    continue;
                }
                assert!(
                    max_channel_delta(left.color, right.color) >= 3
                        || left.color.a != right.color.a,
                    "{} paints {} and {} the same colour",
                    table.name(),
                    left.value,
                    right.value
                );
            }
        }
    }
}
