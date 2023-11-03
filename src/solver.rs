#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// SPP : code based and approximated models
    /// aiming a metric resolution.
    #[default]
    SPP,
    // /// PPP : phase + code based, the ultimate solver
    // /// aiming a millimetric resolution.
    // PPP,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::SPP => write!(fmt, "SPP"),
            // Self::PPP => write!(fmt, "PPP"),
        }
    }
}

use log::{debug, error, warn};
use thiserror::Error;

use hifitime::Epoch;

use nyx::md::prelude::{Arc, Cosm};
use nyx_space::cosmic::eclipse::{eclipse_state, EclipseState};
use nyx_space::cosmic::{Orbit, SPEED_OF_LIGHT};
use nyx_space::md::prelude::{Bodies, Frame, LightTimeCalc};

use gnss::prelude::SV;

use nalgebra::base::{
    DVector,
    MatrixXx4,
    //Vector1,
    //Vector3,
    //Vector4,
};

use crate::apriori::AprioriPosition;
use crate::candidate::Candidate;
use crate::cfg::Config;
use crate::model::TropoComponents;
use crate::model::{
    Modelization,
    Models,
    // Modeling,
};
use crate::solutions::PVTSolution;
use crate::Vector3D;

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("{0} : can't generate a solution")]
    LessThan4SV(Epoch),
    #[error("{0} : failed to invert navigation matrix")]
    SolvingError(Epoch),
    #[error("undefined apriori position")]
    UndefinedAprioriPosition,
    #[error("at least one pseudo range observation is mandatory")]
    NeedsAtLeastOnePseudoRange,
}

/// Interpolation result that your data interpolator should return
/// For Solver.resolve() to truly complete.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct InterpolationResult {
    /// Position in the sky
    pub sky_pos: Vector3D,
    /// Optional elevation compared to reference position and horizon
    pub elevation: Option<f64>,
    /// Optional azimuth compared to reference position and magnetic North
    pub azimuth: Option<f64>,
}

/// PVT Solver
#[derive(Debug, Clone)]
pub struct Solver<I>
where
    I: Fn(Epoch, SV, usize) -> Option<InterpolationResult>,
{
    /// Solver parametrization
    pub cfg: Config,
    /// Type of solver implemented
    pub mode: Mode,
    /// apriori position
    pub apriori: AprioriPosition,
    /// SV state interpolation method. It is mandatory
    /// to resolve the SV state at the requested Epoch otherwise the solver
    /// will not proceed further. User should provide the interpolation method.
    /// Other parameters are SV: Space Vehicle identity we want to resolve, and "usize" interpolation order.
    pub interpolator: I,
    /// cosmic model
    cosmic: Arc<Cosm>,
    /// Earth frame
    earth_frame: Frame,
    /// Sun frame
    sun_frame: Frame,
    /// modelization memory storage
    models: Models,
}

impl<I: std::ops::Fn(Epoch, SV, usize) -> Option<InterpolationResult>> Solver<I> {
    pub fn new(
        mode: Mode,
        apriori: AprioriPosition,
        cfg: &Config,
        interpolator: I,
    ) -> Result<Self, Error> {
        let cosmic = Cosm::de438();
        let sun_frame = cosmic.frame("Sun J2000");
        let earth_frame = cosmic.frame("EME2000");

        /*
         * print some infos on latched config
         */
        if cfg.modeling.iono_delay {
            warn!("can't compensate for ionospheric delay at the moment");
        }

        if cfg.modeling.earth_rotation {
            warn!("can't compensate for earth rotation at the moment");
        }

        if cfg.modeling.relativistic_clock_corr {
            warn!("relativistic clock corr. is not feasible at the moment");
        }

        if mode == Mode::SPP && cfg.min_sv_sunlight_rate.is_some() {
            warn!("eclipse filter is not meaningful when using spp strategy");
        }

        Ok(Self {
            mode,
            cosmic,
            sun_frame,
            earth_frame,
            apriori,
            interpolator,
            cfg: cfg.clone(),
            models: Models::with_capacity(cfg.max_sv),
        })
    }
    /// Candidates election process, you can either call yourself this method
    /// externally prior a Self.run(), or use "pre_selected: false" in Solver.run()
    /// or use "pre_selected: true" with your own selection method prior using Solver.run().
    pub fn elect_candidates(
        t: Epoch,
        pool: Vec<Candidate>,
        mode: Mode,
        cfg: &Config,
    ) -> Vec<Candidate> {
        let mut p = pool.clone();
        p.iter()
            .filter_map(|c| {
                let mode_compliant = match mode {
                    Mode::SPP => true,
                    // Mode::PPP => false, // TODO
                };
                if mode_compliant {
                    Some(c.clone())
                } else {
                    None
                }
            })
            .collect()
    }
    /// Run position solving algorithm, using predefined strategy.
    /// If you want to implement the tropospheric delay compensation yourself,
    /// or have a better source of such components, you can pass them here.
    /// Otherwise, we can always resolve a PVT and rely on internal models.
    pub fn run(
        &mut self,
        t: Epoch,
        pool: Vec<Candidate>,
        tropo_components: Option<TropoComponents>,
    ) -> Result<(Epoch, PVTSolution), Error> {
        let (x0, y0, z0) = (
            self.apriori.ecef.x,
            self.apriori.ecef.y,
            self.apriori.ecef.z,
        );

        let (lat_ddeg, lon_ddeg, altitude_above_sea_m) = (
            self.apriori.geodetic.x,
            self.apriori.geodetic.y,
            self.apriori.geodetic.z,
        );

        let modeling = self.cfg.modeling;
        let interp_order = self.cfg.interp_order;

        let pool = Self::elect_candidates(t, pool, self.mode, &self.cfg);

        /* interpolate positions */
        let mut pool: Vec<Candidate> = pool
            .iter()
            .filter_map(|c| {
                let mut t_tx = c.transmission_time(&self.cfg).ok()?;

                // TODO : complete this equation please
                if self.cfg.modeling.relativistic_clock_corr {
                    let _e = 1.204112719279E-2;
                    let _sqrt_a = 5.153704689026E3;
                    let _sqrt_mu = (3986004.418E8_f64).sqrt();
                    //let dt = -2.0_f64 * sqrt_a * sqrt_mu / SPEED_OF_LIGHT / SPEED_OF_LIGHT * e * elev.sin();
                    // t_tx -=
                }

                // TODO : requires instantaneous speed
                if self.cfg.modeling.earth_rotation {
                    // dt = || rsat - rcvr0 || /c
                    // rsat = R3 * we * dt * rsat
                    // we = 7.2921151467 E-5
                }

                if let Some(interpolated) = (self.interpolator)(t_tx, c.sv, self.cfg.interp_order) {
                    let mut c = c.clone();
                    debug!(
                        "{:?} ({}) : interpolated state: {:?}",
                        t_tx, c.sv, interpolated.sky_pos
                    );
                    c.state = Some(Vector3D {
                        x: interpolated.sky_pos.x * 1.0E3,
                        y: interpolated.sky_pos.y * 1.0E3,
                        z: interpolated.sky_pos.z * 1.0E3,
                    });

                    c.elevation = interpolated.elevation;
                    c.azimuth = interpolated.azimuth;
                    Some(c)
                } else {
                    warn!("{:?} ({}) : interpolation failed", t_tx, c.sv);
                    None
                }
            })
            .collect();

        /* apply elevation filter (if any) */
        if let Some(min_elev) = self.cfg.min_sv_elev {
            for idx in 0..pool.len() - 1 {
                if let Some(elev) = pool[idx].elevation {
                    if elev < min_elev {
                        debug!(
                            "{:?} ({}) : below elevation mask",
                            pool[idx].t, pool[idx].sv
                        );
                        let _ = pool.swap_remove(idx);
                    }
                } else {
                    let _ = pool.swap_remove(idx);
                }
            }
        }

        /* apply eclipse filter (if need be) */
        if let Some(min_rate) = self.cfg.min_sv_sunlight_rate {
            for idx in 0..pool.len() - 1 {
                let state = pool[idx].state.unwrap(); // infaillible
                let (x, y, z) = (state.x, state.y, state.z);
                let orbit = Orbit {
                    x_km: x / 1000.0,
                    y_km: y / 1000.0,
                    z_km: z / 1000.0,
                    vx_km_s: 0.0_f64, // TODO ?
                    vy_km_s: 0.0_f64, // TODO ?
                    vz_km_s: 0.0_f64, // TODO ?
                    epoch: pool[idx].t,
                    frame: self.earth_frame,
                    stm: None,
                };
                let state = eclipse_state(&orbit, self.sun_frame, self.earth_frame, &self.cosmic);
                let eclipsed = match state {
                    EclipseState::Umbra => true,
                    EclipseState::Visibilis => false,
                    EclipseState::Penumbra(r) => r < min_rate,
                };
                if eclipsed {
                    debug!(
                        "{:?} ({}): earth eclipsed, dropping",
                        pool[idx].t, pool[idx].sv
                    );
                    let _ = pool.swap_remove(idx);
                }
            }
        }

        /* make sure we still have enough SV */
        let nb_candidates = pool.len();
        if nb_candidates < 4 {
            return Err(Error::LessThan4SV(t));
        } else {
            debug!("{:?}: {} elected sv", t, nb_candidates);
        }

        /* modelization */
        self.models.modelize(
            t,
            pool.iter().map(|c| (c.sv, c.elevation.unwrap())).collect(),
            lat_ddeg,
            altitude_above_sea_m,
            &self.cfg,
            tropo_components,
        );

        /* form matrix */
        let mut y = DVector::<f64>::zeros(nb_candidates);
        let mut g = MatrixXx4::<f64>::zeros(nb_candidates);

        for (index, c) in pool.iter().enumerate() {
            let sv = c.sv;
            let pr = c.pseudo_range();
            let clock_corr = c.clock_corr.to_seconds();
            let state = c.state.unwrap(); // infaillible
            let (sv_x, sv_y, sv_z) = (state.x, state.y, state.z);

            // let code = data.3;

            let rho = ((sv_x - x0).powi(2) + (sv_y - y0).powi(2) + (sv_z - z0).powi(2)).sqrt();

            let mut models = -clock_corr * SPEED_OF_LIGHT;
            models += self.models.sum_up(sv);

            y[index] = pr - rho - models;

            /*
             * accurate delays compensation (if any)
             */
            // if let Some(int_delay) = self.cfg.internal_delay.get(code) {
            //     y[index] -= int_delay * SPEED_OF_LIGHT;
            // }

            // if let Some(timeref_delay) = self.cfg.time_ref_delay {
            //     y[index] += timeref_delay * SPEED_OF_LIGHT;
            // }

            g[(index, 0)] = (x0 - sv_x) / rho;
            g[(index, 1)] = (y0 - sv_y) / rho;
            g[(index, 2)] = (z0 - sv_z) / rho;
            g[(index, 3)] = 1.0_f64;
        }

        // 7: resolve
        //trace!("y: {} | g: {}", y, g);
        let estimate = PVTSolution::new(g, y);

        if estimate.is_none() {
            Err(Error::SolvingError(t))
        } else {
            Ok((t, estimate.unwrap()))
        }
    }
    /*
     * Evaluates Sun/Earth vector, <!> expressed in Km <!>
     * for all SV NAV Epochs in provided context
     */
    fn sun_earth_vector(&mut self, t: Epoch) -> Vector3D {
        let sun_body = Bodies::Sun;
        let orbit = self.cosmic.celestial_state(
            sun_body.ephem_path(),
            t,
            self.earth_frame,
            LightTimeCalc::None,
        );
        Vector3D {
            x: orbit.x_km * 1000.0,
            y: orbit.y_km * 1000.0,
            z: orbit.z_km * 1000.0,
        }
    }
}
