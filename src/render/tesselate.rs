// MIT/Apache2 License

use super::{double_to_fixed, fixed_to_double};
use crate::auto::render::{Fixed, Linefix, Pointfix, Trapezoid};
use alloc::{collections::VecDeque, vec, vec::Vec};
use core::{
    cmp::Ordering,
    iter::{Fuse, FusedIterator},
    mem,
};

/// Tesselate a shape into a set of trapezoids. This function takes an iterator of points that represent a closed
/// shape, and returns a semi-lazy iterator over the trapezoids.
#[inline]
pub fn tesselate_shape<I: IntoIterator<Item = Pointfix>>(i: I) -> impl Iterator<Item = Trapezoid> {
    // Note: it is more efficient to ignore horizontal edges
    edges_to_trapezoids(
        PointsToEdges {
            inner: i.into_iter().fuse(),
            first: None,
            last: None,
        }
        .filter(|e| e.y1 != e.y2),
    )
}

#[inline]
fn edges_to_trapezoids<I: IntoIterator<Item = Edge>>(i: I) -> Trapezoids {
    let mut edges: Vec<Edge> = i.into_iter().collect();
    if edges.is_empty() {
        // yields nothing
        return Trapezoids {
            y: 0,
            active: vec![],
            inactive: vec![],
            queue: VecDeque::new(),
        };
    }

    // sort and reverse "edges" so it's easy enough to pop edges off
    edges.sort_unstable_by(|e1, e2| match e1.y1.cmp(&e2.y1) {
        Ordering::Equal => e1.x1.cmp(&e2.x1),
        o => o,
    });
    edges.reverse();

    #[cfg(debug_assertions)]
    log::trace!("Edges are: {:?}", &edges);

    Trapezoids {
        y: edges.last().unwrap().y1,
        active: Vec::with_capacity(edges.len()),
        inactive: edges,
        queue: VecDeque::new(),
    }
}

/// Given a set of edges, this iterates over them and produces trapezoids.
struct Trapezoids {
    active: Vec<Edge>,
    inactive: Vec<Edge>,
    y: Fixed,
    queue: VecDeque<Trapezoid>,
}

impl Trapezoids {
    /// Populates `queue` with trapezoids by running one cycle. Returns `false` if it short-circuited.
    #[inline]
    fn populate_queue(&mut self) -> bool {
        log::debug!(
            "Running populate_queue(). There are {} active elements and {} inactive elements",
            self.active.len(),
            self.inactive.len()
        );
        #[cfg(debug_assertions)]
        log::trace!(
            "Creating trapezoids at y: {} ({})",
            fixed_to_double(self.y),
            self.y
        );

        // if both the active and inactive lists are empty, this iterator should stop
        if self.active.is_empty() && self.inactive.is_empty() {
            return false;
        }

        let y = self.y;

        // first, move any qualifying edges into the active group
        while !self.inactive.is_empty() {
            let edge = self.inactive.last().unwrap();
            if edge.y1 > y {
                break;
            }

            // edge qualifies; move it into the active group
            self.active.push(self.inactive.pop().unwrap());
        }

        // compute the x-interception along the current y
        self.active
            .iter_mut()
            .for_each(move |edge| edge.compute_x(y));

        #[cfg(debug_assertions)]
        log::trace!("Active edges are: {:#?}", &self.active);

        // sort the active list by current x intercept
        // likely to be fast since list is close to already sorted and is, for most polygons, smaller than
        // 20 elements
        self.active
            .sort_unstable_by(|e1, e2| match e1.current_x.cmp(&e2.current_x) {
                Ordering::Equal => e1.x2.cmp(&e2.x2),
                o => o,
            });

        // find the next y-level
        let next_y = self
            .active
            .iter()
            .map(|e| e.y2)
            .chain(self.active.windows(2).filter_map(|es| {
                if es[0].x2 > es[1].x2 {
                    Some(es[0].compute_intersect(es[1]) + 1)
                } else {
                    None
                }
            }))
            .chain(self.inactive.last().map(|e| e.y1))
            .min()
            .expect("Iteration should've ended by now");

        // generate trapezoids; push into queue so we return them
        self.queue
            .extend(self.active.chunks_exact(2).map(move |es| {
                let e1 = es[0];
                let e2 = es[1];

                Trapezoid {
                    top: y,
                    bottom: next_y,
                    left: Linefix {
                        p1: Pointfix { x: e1.x1, y: e1.y1 },
                        p2: Pointfix { x: e1.x2, y: e1.y2 },
                    },
                    right: Linefix {
                        p1: Pointfix { x: e2.x1, y: e2.y1 },
                        p2: Pointfix { x: e2.x2, y: e2.y2 },
                    },
                }
            }));

        self.y = next_y;

        // delete now-inactive edges
        self.active.retain(move |e| e.y2 > next_y);

        true
    }
}

impl Iterator for Trapezoids {
    type Item = Trapezoid;

    #[inline]
    fn next(&mut self) -> Option<Trapezoid> {
        loop {
            // if there are any leftover trapezoids in the queue, return one
            if let Some(trap) = self.queue.pop_front() {
                return Some(trap);
            }

            // otherwise, try to generate some
            if !self.populate_queue() {
                return None;
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some((self.active.len() + self.inactive.len()).pow(2)))
    }

    // Implement fold(), since a lot of functions use it and we can do it a bit more efficiently.
    // TODO: also implement try_fold() once the Try trait becomes stable

    #[inline]
    fn fold<B, F: FnMut(B, Trapezoid) -> B>(mut self, init: B, f: F) -> B {
        // populate the queue as much as we can
        while self.populate_queue() {}

        // drain the queue using the closure
        self.queue.into_iter().fold(init, f)
    }
}

impl FusedIterator for Trapezoids {}

/// Iterate over a set of points, transforming them into a set of edges.
struct PointsToEdges<I> {
    inner: Fuse<I>,
    first: Option<Pointfix>,
    last: Option<Pointfix>,
}

impl<I: Iterator<Item = Pointfix>> Iterator for PointsToEdges<I> {
    type Item = Edge;

    #[inline]
    fn next(&mut self) -> Option<Edge> {
        loop {
            match self.inner.next() {
                Some(pt) => {
                    // we have a point. if this is the first point, store it in "first" and "last". otherwise,
                    // just store it in "last" and return the combination of this point and the former last point
                    match mem::replace(&mut self.last, Some(pt)) {
                        None => {
                            self.first = Some(pt);
                        }
                        Some(last) => {
                            return Some(Edge::from_points(last, pt));
                        }
                    }
                }
                None => {
                    // if "first" is none, or if "first" and "last" are equal, return None
                    // otherwise, combine "first" and "last
                    match (self.first.take(), self.last.take()) {
                        (Some(first), Some(last)) => {
                            if first == last {
                                return None;
                            } else {
                                return Some(Edge::from_points(last, first));
                            }
                        }
                        _ => return None,
                    }
                }
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        #[inline]
        fn cvt_size(s: usize) -> usize {
            match s {
                0 | 1 => 0,
                s => s,
            }
        }

        let (lo, hi) = self.inner.size_hint();
        (cvt_size(lo), hi.map(cvt_size))
    }
}

impl<I: Iterator<Item = Pointfix>> FusedIterator for PointsToEdges<I> {}
impl<I: Iterator<Item = Pointfix> + ExactSizeIterator> ExactSizeIterator for PointsToEdges<I> {}

/// An edge between two points.
#[derive(Debug, Copy, Clone)]
struct Edge {
    x1: Fixed,
    y1: Fixed,
    x2: Fixed,
    y2: Fixed,
    current_x: Fixed,
}

impl Edge {
    #[inline]
    fn from_points(p1: Pointfix, p2: Pointfix) -> Edge {
        if p1.y < p2.y {
            Edge {
                x1: p1.x,
                y1: p1.y,
                x2: p2.x,
                y2: p2.y,
                current_x: 0,
            }
        } else {
            Edge {
                x1: p2.x,
                y1: p2.y,
                x2: p1.x,
                y2: p1.y,
                current_x: 0,
            }
        }
    }
}

impl Edge {
    #[inline]
    fn inverse_slope(self) -> f64 {
        fixed_to_double(self.x2 - self.x1) / fixed_to_double(self.y2 - self.y1)
    }

    #[inline]
    fn x_intercept(self) -> f64 {
        fixed_to_double(self.x1) - (self.inverse_slope() * fixed_to_double(self.y1))
    }

    #[inline]
    fn compute_x(&mut self, y: Fixed) {
        let dx = self.x2 - self.x1;
        let ex = (y - self.y1) as f64 * (dx as f64);
        let dy = self.y2 - self.y1;
        self.current_x = self.x1 + ((ex / dy as f64) as Fixed);
    }

    #[inline]
    fn compute_intersect(self, other: Edge) -> Fixed {
        let m1 = self.inverse_slope();
        let b1 = self.x_intercept();
        let m2 = other.inverse_slope();
        let b2 = other.x_intercept();
        double_to_fixed((b2 - b1) / (m2 - m1))
    }
}
