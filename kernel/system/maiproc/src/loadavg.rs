//! Charge moyenne du système (`/proc/loadavg`, `sysinfo`), sur 1, 5 et 15 minutes.
//!
//! Même filtre que `calc_load` de Linux (moyennes exponentielles en virgule fixe sur 11 bits, un
//! pas toutes les 5 s), mais alimenté par l'intégrale du nombre de tâches prêtes tenue par
//! l'ordonnanceur (`runnable_ns`) : la valeur d'un pas est la moyenne exacte sur ses 5 s, pas un
//! instantané pris au moment du tick (biaisé par les tâches qui se réveillent avec lui). Calcul
//! paresseux : les pas écoulés depuis la dernière lecture sont appliqués à la lecture suivante.

use spin::Mutex;

const FSHIFT: u32 = 11;
const FIXED_1: u64 = 1 << FSHIFT;
/// e^(-5/60), e^(-5/300), e^(-5/900) en virgule fixe.
const EXP: [u64; 3] = [1884, 2014, 2037];
const STEP_NS: u64 = 5_000_000_000;
/// Au-delà d'une heure sans lecture, les anciennes valeurs sont oubliées.
const MAX_STEPS: u64 = 720;

struct State {
    last_ns: u64,
    last_runnable: u64,
    avenrun: [u64; 3],
}

static STATE: Mutex<State> = Mutex::new(State { last_ns: 0, last_runnable: 0, avenrun: [0; 3] });

fn sample() -> (u64, u64) {
    let st = task::scheduler::stats();
    let now = st.iter().map(|s| s.now_ns).max().unwrap_or(0);
    let runnable = st.iter().map(|s| s.runnable_ns).sum();
    (now, runnable)
}

/// Charges moyennes ×100 (1, 5, 15 minutes).
pub fn get() -> [u64; 3] {
    let (now, runnable) = sample();
    let mut s = STATE.lock();
    if s.last_ns == 0 {
        s.last_ns = now;
        s.last_runnable = runnable;
    }
    let dt = now.saturating_sub(s.last_ns);
    let steps = dt / STEP_NS;
    if steps > 0 {
        // Nombre moyen de tâches prêtes sur l'intervalle, en virgule fixe.
        let avg = ((runnable.saturating_sub(s.last_runnable) as u128 * FIXED_1 as u128) / dt.max(1) as u128) as u64;
        for _ in 0..steps.min(MAX_STEPS) {
            for (a, e) in s.avenrun.iter_mut().zip(EXP) {
                *a = (*a * e + avg * (FIXED_1 - e) + FIXED_1 / 2) >> FSHIFT;
            }
        }
        s.last_ns = now;
        s.last_runnable = runnable;
    }
    s.avenrun.map(|a| (a * 100 + FIXED_1 / 2) >> FSHIFT)
}
