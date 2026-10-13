//! 지연 히스토그램. 마이크로초 값을 로그 간격 구간에 센다(상대 오차 1% 미만).
//! 값을 모두 저장하지 않으므로 장시간 시험에서도 메모리가 늘지 않는다.

const SUB_BITS: u32 = 7;
const SUB: u64 = 1 << SUB_BITS;
/// 2^40 마이크로초(약 12.7일)까지. 넘으면 마지막 구간에 센다.
const MAX_EXP: u32 = 40;
const BUCKETS: usize = (SUB as usize) * (MAX_EXP - SUB_BITS + 2) as usize;

#[derive(Clone)]
pub struct Hist {
    counts: Vec<u64>,
    n: u64,
    max: u64,
    sum: u128,
}

impl Default for Hist {
    fn default() -> Self {
        Hist {
            counts: vec![0; BUCKETS],
            n: 0,
            max: 0,
            sum: 0,
        }
    }
}

fn index(v: u64) -> usize {
    if v < SUB {
        return v as usize;
    }
    let e = 63 - v.leading_zeros();
    let e = e.min(MAX_EXP);
    let shift = e - SUB_BITS;
    let top = (v >> shift).min(2 * SUB - 1);
    (SUB + u64::from(shift) * SUB + (top - SUB)) as usize
}

/// 구간의 윗경계(구간에 든 값은 이 값 이하).
fn upper(idx: usize) -> u64 {
    let idx = idx as u64;
    if idx < SUB {
        return idx;
    }
    let k = idx - SUB;
    let shift = k / SUB;
    let top = SUB + k % SUB;
    ((top + 1) << shift) - 1
}

impl Hist {
    pub fn record(&mut self, micros: u64) {
        self.counts[index(micros)] += 1;
        self.n += 1;
        self.sum += u128::from(micros);
        self.max = self.max.max(micros);
    }

    pub fn merge(&mut self, other: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += *b;
        }
        self.n += other.n;
        self.sum += other.sum;
        self.max = self.max.max(other.max);
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        self.counts.iter_mut().for_each(|c| *c = 0);
        self.n = 0;
        self.sum = 0;
        self.max = 0;
    }

    pub fn count(&self) -> u64 {
        self.n
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    pub fn mean(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum as f64 / self.n as f64
        }
    }

    /// `q`(0..=1) 분위 값. 비어 있으면 0. 최댓값을 넘지 않는다.
    pub fn quantile(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let target = ((self.n as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= target {
                return if i == BUCKETS - 1 {
                    self.max
                } else {
                    upper(i).min(self.max)
                };
            }
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 작은_값은_정확하다() {
        let mut h = Hist::default();
        for v in 1..=100 {
            h.record(v);
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.quantile(0.5), 50);
        assert_eq!(h.quantile(0.99), 99);
        assert_eq!(h.max(), 100);
    }

    #[test]
    fn 큰_값도_오차가_1퍼센트_미만이다() {
        let mut h = Hist::default();
        for v in [1_000u64, 12_345, 987_654, 25_000_000] {
            h.record(v);
            let got = h.quantile(1.0);
            assert!(got <= v, "최댓값을 넘지 않는다: {got} > {v}");
            assert!((v - got) as f64 / v as f64 <= 0.01, "{v} -> {got}",);
            h.clear();
        }
    }

    #[test]
    fn 합치면_분위가_이어진다() {
        let mut a = Hist::default();
        let mut b = Hist::default();
        for v in 1..=50 {
            a.record(v * 1000);
        }
        for v in 51..=100 {
            b.record(v * 1000);
        }
        a.merge(&b);
        assert_eq!(a.count(), 100);
        let p50 = a.quantile(0.5);
        assert!((49_500..=50_500).contains(&p50), "{p50}");
        assert_eq!(a.max(), 100_000);
    }

    #[test]
    fn 비어_있으면_0이다() {
        let h = Hist::default();
        assert_eq!(h.quantile(0.99), 0);
        assert_eq!(h.mean(), 0.0);
    }

    #[test]
    fn 범위를_넘는_값도_센다() {
        let mut h = Hist::default();
        h.record(u64::MAX / 4);
        assert_eq!(h.count(), 1);
        assert_eq!(h.quantile(1.0), u64::MAX / 4);
    }
}
