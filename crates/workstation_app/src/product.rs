use radar_core::{MomentType, ProductId, RadarVolume};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DisplayProduct {
    #[default]
    Reflectivity,
    Velocity,
    DealiasedVelocity,
    StormRelativeVelocity,
    DealiasedStormRelativeVelocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
}

impl DisplayProduct {
    pub const ALL: [Self; 10] = [
        Self::Reflectivity,
        Self::Velocity,
        Self::DealiasedVelocity,
        Self::StormRelativeVelocity,
        Self::DealiasedStormRelativeVelocity,
        Self::SpectrumWidth,
        Self::DifferentialReflectivity,
        Self::CorrelationCoefficient,
        Self::DifferentialPhase,
        Self::SpecificDifferentialPhase,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Reflectivity => "REF",
            Self::Velocity => "VEL",
            Self::DealiasedVelocity => "DVEL",
            Self::StormRelativeVelocity => "SRV",
            Self::DealiasedStormRelativeVelocity => "DSRV",
            Self::SpectrumWidth => "SW",
            Self::DifferentialReflectivity => "ZDR",
            Self::CorrelationCoefficient => "RHO",
            Self::DifferentialPhase => "PHI",
            Self::SpecificDifferentialPhase => "KDP",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Reflectivity => "Reflectivity",
            Self::Velocity => "Base Velocity",
            Self::DealiasedVelocity => "Dealiased Velocity",
            Self::StormRelativeVelocity => "Storm-Relative Velocity",
            Self::DealiasedStormRelativeVelocity => "Dealiased SRV",
            Self::SpectrumWidth => "Spectrum Width",
            Self::DifferentialReflectivity => "Differential Reflectivity",
            Self::CorrelationCoefficient => "Correlation Coefficient",
            Self::DifferentialPhase => "Differential Phase",
            Self::SpecificDifferentialPhase => "Specific Differential Phase",
        }
    }

    pub fn product_id(self) -> ProductId {
        ProductId(self.id().to_owned())
    }

    pub fn from_product_id(id: &ProductId) -> Self {
        match id.0.as_str() {
            "VEL" => Self::Velocity,
            "DVEL" => Self::DealiasedVelocity,
            "SRV" => Self::StormRelativeVelocity,
            "DSRV" => Self::DealiasedStormRelativeVelocity,
            "SW" => Self::SpectrumWidth,
            "ZDR" => Self::DifferentialReflectivity,
            "RHO" => Self::CorrelationCoefficient,
            "PHI" => Self::DifferentialPhase,
            "KDP" => Self::SpecificDifferentialPhase,
            _ => Self::Reflectivity,
        }
    }

    pub fn source_moment(self) -> MomentType {
        match self {
            Self::Reflectivity => MomentType::Reflectivity,
            Self::Velocity
            | Self::DealiasedVelocity
            | Self::StormRelativeVelocity
            | Self::DealiasedStormRelativeVelocity => MomentType::Velocity,
            Self::SpectrumWidth => MomentType::SpectrumWidth,
            Self::DifferentialReflectivity => MomentType::DifferentialReflectivity,
            Self::CorrelationCoefficient => MomentType::CorrelationCoefficient,
            Self::DifferentialPhase => MomentType::DifferentialPhase,
            Self::SpecificDifferentialPhase => MomentType::SpecificDifferentialPhase,
        }
    }

    pub const fn uses_dealiased_velocity(self) -> bool {
        matches!(
            self,
            Self::DealiasedVelocity | Self::DealiasedStormRelativeVelocity
        )
    }

    pub const fn is_storm_relative(self) -> bool {
        matches!(
            self,
            Self::StormRelativeVelocity | Self::DealiasedStormRelativeVelocity
        )
    }

    pub fn is_available_in_cut(self, volume: &RadarVolume, cut_index: usize) -> bool {
        volume
            .cuts
            .get(cut_index)
            .is_some_and(|cut| cut.moments.contains_key(&self.source_moment()))
    }

    pub fn first_available_cut(self, volume: &RadarVolume) -> Option<usize> {
        volume
            .cuts
            .iter()
            .position(|cut| cut.moments.contains_key(&self.source_moment()))
    }

    pub fn next_available_cut(
        self,
        volume: &RadarVolume,
        current: usize,
        delta: isize,
    ) -> Option<usize> {
        if volume.cuts.is_empty() {
            return None;
        }
        let mut index = current.min(volume.cuts.len() - 1) as isize;
        loop {
            index = index.saturating_add(delta);
            if index < 0 || index >= volume.cuts.len() as isize {
                return None;
            }
            let candidate = index as usize;
            if self.is_available_in_cut(volume, candidate) {
                return Some(candidate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_ids_round_trip() {
        for product in DisplayProduct::ALL {
            assert_eq!(
                DisplayProduct::from_product_id(&product.product_id()),
                product
            );
        }
    }

    #[test]
    fn velocity_variants_share_velocity_source() {
        assert_eq!(
            DisplayProduct::DealiasedStormRelativeVelocity.source_moment(),
            MomentType::Velocity
        );
    }
}
