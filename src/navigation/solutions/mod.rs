//! PVT Solutions
// use crate::Error;
use crate::prelude::{Vector3, SV};
use std::collections::HashMap;
// use crate::solver::{FilterState, LSQState};

// use nyx::cosmic::SPEED_OF_LIGHT;
use super::SVInput;
use nalgebra::base::Matrix3;
use nalgebra::base::Matrix4;

pub(crate) mod validator;

#[derive(Debug, Copy, Clone, Default)]
pub enum PVTSolutionType {
    /// Default, complete solution with Position,
    /// Velocity and Time components. Requires either
    /// 4 vehicles in sight, or 3 if you're working in fixed altitude
    /// (provided ahead of time).
    #[default]
    PositionVelocityTime,
    /// Resolve Time component only. Requires 1 vehicle to resolve.
    TimeOnly,
}

impl std::fmt::Display for PVTSolutionType {
    /*
     * Prints self
     */
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::PositionVelocityTime => write!(f, "PVT"),
            Self::TimeOnly => write!(f, "TimeOnly"),
        }
    }
}

/// PVT Solution, always expressed as the correction to apply
/// to an Apriori / static position.
#[derive(Debug, Clone)]
// #[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PVTSolution {
    /// Position errors (in [m] ECEF)
    pub pos: Vector3<f64>,
    /// Absolute Velocity (in [m/s] ECEF).
    pub vel: Vector3<f64>,
    /// Time correction in [s]
    pub dt: f64,
    /// Space Vehicles that helped form this solution
    /// and data associated to each individual SV
    pub sv: HashMap<SV, SVInput>,
    /// Geometric Dilution of Precision
    pub gdop: f64,
    /// Time Dilution of Precision
    pub tdop: f64,
    /// Position Dilution of Precision
    pub pdop: f64,
    // Q
    pub(crate) q: Matrix4<f64>,
}

impl PVTSolution {
    /// Returns list of Space Vehicles (SV) that help form this solution.
    pub fn sv(&self) -> Vec<SV> {
        self.sv.keys().copied().collect()
    }
    fn q_enu(&self, lat: f64, lon: f64) -> Matrix3<f64> {
        let r = Matrix3::<f64>::new(
            -lon.sin(),
            -lon.cos() * lat.sin(),
            lat.cos() * lon.cos(),
            lon.cos(),
            -lat.sin() * lon.sin(),
            lat.cos() * lon.sin(),
            0.0_f64,
            lat.cos(),
            lon.sin(),
        );
        let q_3 = Matrix3::<f64>::new(
            self.q[(0, 0)],
            self.q[(0, 1)],
            self.q[(0, 2)],
            self.q[(1, 0)],
            self.q[(1, 1)],
            self.q[(1, 2)],
            self.q[(2, 0)],
            self.q[(2, 1)],
            self.q[(2, 2)],
        );
        r.clone().transpose() * q_3 * r
    }
    pub fn hdop(&self, lat: f64, lon: f64) -> f64 {
        let q = self.q_enu(lat, lon);
        (q[(0, 0)] + q[(1, 1)]).sqrt()
    }
    pub fn vdop(&self, lat: f64, lon: f64) -> f64 {
        self.q_enu(lat, lon)[(2, 2)].sqrt()
    }
}