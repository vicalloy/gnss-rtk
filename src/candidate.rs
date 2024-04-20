//! Position solving candidate

use gnss::prelude::SV;
use hifitime::Unit;
use itertools::Itertools;
use log::debug;
use std::cmp::Ordering;

use nyx::cosmic::SPEED_OF_LIGHT;
use nyx::linalg::{DVector, MatrixXx4};

use crate::prelude::{Config, Duration, Epoch, InterpolationResult, Vector3};
use crate::solutions::{PVTBias, PVTSVData};
use crate::{
    bias,
    bias::{IonosphericBias, TropoModel, TroposphericBias},
    prelude::Method,
    Error,
};

/// Signal observation to attach to each candidate
#[derive(Debug, Default, PartialEq, Clone)]
pub struct Observation {
    /// carrier frequency [Hz]
    pub frequency: f64,
    /// actual observation
    pub value: f64,
    /// optional (but recommended) SNR in [dB]
    pub snr: Option<f64>,
}

/// Position solving candidate
#[derive(Debug, Clone)]
pub struct Candidate {
    /// SV
    pub sv: SV,
    /// Sampling Epoch
    pub t: Epoch,
    /// Tx Epoch
    pub t_tx: Epoch,
    /// state that needs to be resolved
    pub state: Option<InterpolationResult>,
    // SV group delay
    pub(crate) tgd: Option<Duration>,
    // SV clock state (compared to GNSS timescale)
    pub(crate) clock_state: Vector3<f64>,
    // SV clock correction
    pub(crate) clock_corr: Duration,
    // Code observations
    pub(crate) code: Vec<Observation>,
    // Phase observations
    pub(crate) phase: Vec<Observation>,
    // Doppler observations
    pub(crate) doppler: Vec<Observation>,
}

impl Candidate {
    /// Creates a new candidate, to inject in the solver pool.
    /// SV : satellite vehicle (identity).
    /// t: Epoch at which the signals were sampled.
    /// clock_state: SV clock state.
    /// clock_corr: current clock correction (mandatory).
    /// "tgd": possible group delay
    /// "code": provide as many observations as you can
    /// "phase": provide as many observations as you can
    /// "doppler": provide as many observations as you can
    pub fn new(
        sv: SV,
        t: Epoch,
        clock_state: Vector3<f64>,
        clock_corr: Duration,
        tgd: Option<Duration>,
        code: Vec<Observation>,
        phase: Vec<Observation>,
        doppler: Vec<Observation>,
    ) -> Result<Self, Error> {
        if code.is_empty() {
            // TODO check this outside, and base on current strategy
            Err(Error::NeedsAtLeastOnePseudoRange)
        } else {
            Ok(Self {
                sv,
                t,
                t_tx: t,
                clock_state,
                clock_corr,
                tgd,
                code,
                phase,
                doppler,
                state: None,
            })
        }
    }
    /*
     * Returns best observed SNR, whatever the signal
     */
    pub(crate) fn best_snr(&self) -> Option<f64> {
        self.code
            .iter()
            .chain(self.phase.iter())
            .chain(self.doppler.iter())
            .max_by(|a, b| {
                if let Some(snr_a) = a.snr {
                    if let Some(snr_b) = b.snr {
                        snr_a.partial_cmp(&snr_b).unwrap()
                    } else {
                        Ordering::Greater
                    }
                } else {
                    Ordering::Less
                }
            })
            .map(|c| c.snr)?
    }
    /*
     * Returns one pseudo range observation [m], whatever the frequency.
     * Best SNR is preferred though (if such information was provided).
     */
    pub(crate) fn prefered_pseudorange(&self) -> Option<Observation> {
        let mut snr = Option::<f64>::None;
        let mut code = Option::<Observation>::None;
        for c in &self.code {
            if code.is_none() {
                code = Some(c.clone());
                snr = c.snr;
            } else {
                // prefer best SNR if possible
                if let Some(s1) = c.snr {
                    if snr.is_some() {
                        let s2 = snr.unwrap();
                        if s1 > s2 {
                            snr = Some(s1);
                            code = Some(c.clone());
                        }
                    } else {
                        snr = Some(s1);
                        code = Some(c.clone());
                    }
                }
            }
        }
        code
    }
    /*
     * Returns true if we're ppp compatible
     */
    pub(crate) fn ppp_compatible(&self) -> bool {
        self.dual_pseudorange() // && self.dual_phase() //TODO
    }
    pub(crate) fn dual_pseudorange(&self) -> bool {
        self.code
            .iter()
            .map(|c| (c.frequency / 1000.0) as u16)
            .unique()
            .count()
            > 1
    }
    pub(crate) fn dual_phase(&self) -> bool {
        self.phase
            .iter()
            .map(|c| (c.frequency / 1000.0) as u16)
            .unique()
            .count()
            > 1
    }
    /*
     * Forms combination
     */
    pub(crate) fn pseudorange_combination(&self) -> Option<Observation> {
        let mut codes = (
            Option::<&Observation>::None,
            Option::<&Observation>::None,
            Option::<&Observation>::None,
        );
        for code in &self.code {
            let freq = (code.frequency / 1.0E6) as u16;
            if freq == 1575 {
                codes.0 = Some(code);
            } else if freq == 1227 {
                codes.1 = Some(code);
            } else if freq == 1176 {
                codes.2 = Some(code);
            }
        }

        let c_l1 = codes.0?;
        let f_l1 = 1575.42_f64 * 1.0E6_f64;

        let (c_lx, f_lx) = match codes.1 {
            Some(pr) => (pr, 1227.6_f64 * 1.0E6_f64),
            None => match codes.2 {
                Some(pr) => (pr, 1176.45_f64 * 1.0E6_f64),
                None => {
                    return None;
                },
            },
        };

        let alpha = 1.0 / (f_l1.powi(2) - f_lx.powi(2));
        let beta = f_l1.powi(2);
        let gamma = f_lx.powi(2);
        Some({
            Observation {
                snr: None,
                frequency: c_l1.frequency,
                value: alpha * (beta * c_l1.value - gamma * c_lx.value),
            }
        })
    }
    /*
     * apply min SNR mask
     */
    pub(crate) fn min_snr_mask(&mut self, min_snr: f64) {
        self.code.retain(|c| {
            if let Some(snr) = c.snr {
                snr >= min_snr
            } else {
                false
            }
        });
        self.doppler.retain(|d| {
            if let Some(snr) = d.snr {
                snr >= min_snr
            } else {
                false
            }
        });
        self.phase.retain(|p| {
            if let Some(snr) = p.snr {
                snr >= min_snr
            } else {
                false
            }
        });
    }
    /*
     * Computes signal transmission Epoch
     * returns (t_tx, dt_ttx)
     * "t_tx": Epoch in given timescale
     * "dt_ttx": elapsed duration in seconds in given timescale
     * "frame": Solid body reference Frame
     */
    pub(crate) fn transmission_time(&self, cfg: &Config) -> Result<(Epoch, Duration), Error> {
        let (t, ts) = (self.t, self.t.time_scale);
        let seconds_ts = t.to_duration().to_seconds();

        let dt_tx = seconds_ts
            - self
                .prefered_pseudorange()
                .ok_or(Error::MissingPseudoRange)?
                .value
                / SPEED_OF_LIGHT;

        let mut e_tx = Epoch::from_duration(dt_tx * Unit::Second, ts);

        if cfg.modeling.sv_clock_bias {
            debug!(
                "{:?} ({}) clock correction: {}",
                t, self.sv, self.clock_corr
            );
            e_tx -= self.clock_corr;
        }

        if cfg.modeling.sv_total_group_delay {
            if let Some(tgd) = self.tgd {
                debug!("{:?} ({}) tgd   : {}", t, self.sv, tgd);
                e_tx -= tgd;
            }
        }

        let dt_secs = (t - e_tx).to_seconds();
        let dt = Duration::from_seconds(dt_secs);
        assert!(
            dt_secs > 0.0,
            "physical non sense - RX {:?} prior TX {:?}",
            t,
            e_tx
        );
        assert!(
            dt_secs <= 0.1,
            "something's wrong - {} propagation delay is suspicious",
            dt
        );
        Ok((e_tx, dt))
    }
    /*
     * Resolves Self
     */
    pub(crate) fn resolve(
        &self,
        t: Epoch,
        cfg: &Config,
        apriori: (f64, f64, f64),
        apriori_geo: (f64, f64, f64),
        iono_bias: &IonosphericBias,
        tropo_bias: &TroposphericBias,
        row_index: usize,
        y: &mut DVector<f64>,
        g: &mut MatrixXx4<f64>,
    ) -> Result<PVTSVData, Error> {
        // state
        let state = self.state.ok_or(Error::UnresolvedState)?;
        let clock_corr = self.clock_corr.to_seconds();
        let (azimuth, elevation) = (state.azimuth, state.elevation);

        /*
         * compensate for ARP (if possible)
         */
        let apriori = match cfg.arp_enu {
            Some(offset) => (
                apriori.0 + offset.0,
                apriori.1 + offset.1,
                apriori.2 + offset.2,
            ),
            None => apriori,
        };

        let (x0, y0, z0) = apriori;
        let (sv_x, sv_y, sv_z) = (state.position[0], state.position[1], state.position[2]);

        let mut sv_data = PVTSVData::default();
        sv_data.azimuth = azimuth;
        sv_data.elevation = elevation;

        let rho = ((sv_x - x0).powi(2) + (sv_y - y0).powi(2) + (sv_z - z0).powi(2)).sqrt();
        g[(row_index, 0)] = (x0 - sv_x) / rho;
        g[(row_index, 1)] = (y0 - sv_y) / rho;
        g[(row_index, 2)] = (z0 - sv_z) / rho;
        g[(row_index, 3)] = 1.0_f64;

        let mut models = 0.0_f64;
        if cfg.modeling.sv_clock_bias {
            models -= clock_corr * SPEED_OF_LIGHT;
        }

        /*
         * Possible delay compensations
         */
        if let Some(delay) = cfg.externalref_delay {
            y[row_index] -= delay * SPEED_OF_LIGHT;
        }

        let code = match cfg.method {
            Method::SPP => self
                .prefered_pseudorange()
                .ok_or(Error::MissingPseudoRange)?,
            Method::PPP => self
                .pseudorange_combination()
                .ok_or(Error::PseudoRangeCombination)?,
        };

        let (pr, frequency) = (code.value, code.frequency);

        /*
         * IONO + TROPO biases
         */
        let rtm = bias::RuntimeParam {
            t,
            elevation,
            azimuth,
            frequency,
            apriori_geo,
        };

        /*
         * TROPO
         */
        if cfg.modeling.tropo_delay {
            if tropo_bias.needs_modeling() {
                let bias = TroposphericBias::model(TropoModel::Niel, &rtm);
                debug!("{:?} : modeled tropo delay {:.3E}[m]", t, bias);
                models += bias;
                sv_data.tropo_bias = PVTBias::modeled(bias);
            } else if let Some(bias) = tropo_bias.bias(&rtm) {
                debug!("{:?} : measured tropo delay {:.3E}[m]", t, bias);
                models += bias;
                sv_data.tropo_bias = PVTBias::measured(bias);
            }
        }

        /*
         * IONO
         */
        if cfg.method == Method::SPP {
            if cfg.modeling.iono_delay {
                if let Some(bias) = iono_bias.bias(&rtm) {
                    debug!(
                        "{:?} : modeled iono delay (f={:.3E}Hz) {:.3E}[m]",
                        t, rtm.frequency, bias
                    );
                    models += bias;
                    sv_data.iono_bias = PVTBias::modeled(bias);
                }
            }
        }

        /*
         * Possible frequency dependent delays
         */
        for delay in &cfg.int_delay {
            if delay.frequency == frequency {
                y[row_index] += delay.delay * SPEED_OF_LIGHT;
            }
        }

        y[row_index] = pr - rho - models;
        Ok(sv_data)
    }
}

#[cfg(test)]
mod test {
    use crate::prelude::{Candidate, Duration, Epoch, Observation, Vector3, SV};
    #[test]
    fn prefered_pseudorange() {
        let l1_freq = 1575.42_f64 * 1.0E6_f64;
        let l2_freq = 1176.45_f64 * 1.0E6_f64;
        let l5_freq = 1176.45_f64 * 1.0E6_f64;
        let codes = vec![
            Observation {
                value: 1.0,
                snr: None,
                frequency: l1_freq,
            },
            Observation {
                value: 2.0,
                snr: None,
                frequency: l2_freq,
            },
            Observation {
                value: 3.0,
                snr: None,
                frequency: l5_freq,
            },
        ];
        let cd = Candidate::new(
            SV::default(),
            Epoch::default(),
            Vector3::<f64>::default(),
            Duration::default(),
            codes,
            vec![],
            vec![],
        )
        .unwrap();
        assert_eq!(
            cd.prefered_pseudorange(),
            Some(Observation {
                value: 1.0,
                snr: None,
                frequency: l1_freq,
            })
        );
    }
    #[test]
    fn best_snr() {
        let l1_freq = 1575.42_f64 * 1.0E6_f64;
        let l2_freq = 1176.45_f64 * 1.0E6_f64;
        let l5_freq = 1176.45_f64 * 1.0E6_f64;

        let codes = vec![
            Observation {
                value: 1.0,
                snr: None,
                frequency: l1_freq,
            },
            Observation {
                value: 2.0,
                snr: None,
                frequency: l2_freq,
            },
            Observation {
                value: 3.0,
                snr: Some(10.0),
                frequency: l5_freq,
            },
            Observation {
                value: 4.0,
                snr: Some(11.0),
                frequency: l2_freq,
            },
            Observation {
                value: 5.0,
                snr: Some(9.0),
                frequency: l2_freq,
            },
        ];
        let cd = Candidate::new(
            SV::default(),
            Epoch::default(),
            Vector3::<f64>::default(),
            Duration::default(),
            codes,
            vec![],
            vec![],
        )
        .unwrap();
        assert_eq!(cd.best_snr(), Some(11.0));

        let codes = vec![
            Observation {
                value: 1.0,
                snr: Some(1.0),
                frequency: l1_freq,
            },
            Observation {
                value: 2.0,
                snr: Some(1.1),
                frequency: l2_freq,
            },
            Observation {
                value: 3.0,
                snr: Some(1.2),
                frequency: l5_freq,
            },
        ];
        let cd = Candidate::new(
            SV::default(),
            Epoch::default(),
            Vector3::<f64>::default(),
            Duration::default(),
            codes,
            vec![],
            vec![],
        )
        .unwrap();
        assert_eq!(cd.best_snr(), Some(1.2));
    }
}
