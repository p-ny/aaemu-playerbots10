//! The movement controller walking navmesh routes: planning inline and on the pool, following,
//! the fallbacks, the stuck replans and the exact updates before range-checked packets.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aa_proto::cs::Out;
use aa_proto::opcodes::cs as op;
use aa_world::nav::{NavMesh, Path, PathResult};
use tokio::sync::mpsc::Receiver;

use super::testmesh::{self, FlatWorld, FLOOR_Z};
use super::{NavLink, NavMetrics, NavWorld, Planner};
use crate::bot::cmd::BotCmd;
use crate::bot::motion::{
    Ground, Motion, MotionCfg, MotionEnv, MotionStatus, MoveFailure, Pace, TRIP_TIMEOUT_MS,
};
use crate::bot::place::Place;
use crate::hub::BotSlot;

const T0: u64 = 1_000;
const CELL: f32 = 5.0;

/// 200 x 100 m; a wall at x 100..105 from y 0 to 80 with a gap above it.
fn detour_mesh() -> NavMesh {
    testmesh::grid((0.0, 0.0), (40, 20), CELL, |cx, cy| cx == 20 && cy <= 15)
}

fn in_detour_wall(p: [f32; 3]) -> bool {
    (100.0..105.0).contains(&p[0]) && (0.0..80.0).contains(&p[1])
}

/// 600 x 60 m; a wall at x 300..305 from y 0 to 45.
fn long_mesh() -> NavMesh {
    testmesh::grid((0.0, 0.0), (120, 12), CELL, |cx, cy| cx == 60 && cy <= 8)
}

struct Rig {
    motion: Motion,
    out: Vec<Out>,
    rng: fastrand::Rng,
    now: u64,
    metrics: Arc<NavMetrics>,
    rx: Receiver<BotCmd>,
    ground: Box<dyn Ground>,
}

struct Opts {
    pool: bool,
    enabled: bool,
    terrain: bool,
    run_speed: f32,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            pool: false,
            enabled: true,
            terrain: true,
            run_speed: 5.4,
        }
    }
}

impl Rig {
    fn new(mesh: NavMesh, pos: [f32; 3], o: Opts) -> Rig {
        let metrics = Arc::new(NavMetrics::default());
        let slot = BotSlot::new(pb_web::BotSnapshot::default());
        let rx = slot.attach();
        let mut world = FlatWorld::new(mesh);
        if !o.terrain {
            Arc::get_mut(&mut world).unwrap().terrain = None;
        }
        let world: Arc<dyn NavWorld> = world;
        let planner = o
            .pool
            .then(|| Arc::new(Planner::start(1, 16, metrics.clone())));
        let mut motion = Motion::new(MotionCfg {
            run_speed: o.run_speed,
            walk_speed: 1.8,
        });
        motion.attach_nav(NavLink {
            enabled: o.enabled,
            world,
            planner,
            reply: slot,
            metrics: metrics.clone(),
            main: "main_world".into(),
            seed: 7,
        });
        motion.enter_world(0x400, pos, 0.0, T0);
        let terrain = o.terrain;
        Rig {
            motion,
            out: Vec::new(),
            rng: fastrand::Rng::with_seed(4),
            now: T0,
            metrics,
            rx,
            ground: Box::new(move |_: f32, _: f32| terrain.then_some(FLOOR_Z)),
        }
    }

    fn tick(&mut self, dt: u64) -> MotionStatus {
        self.tick_gap(dt, 250)
    }

    fn tick_gap(&mut self, dt: u64, gap: u64) -> MotionStatus {
        self.now += dt;
        let mut env = MotionEnv {
            ground: &*self.ground,
            now_ms: self.now,
            out: &mut self.out,
            rng: &mut self.rng,
            send_gap_ms: gap,
            combat: false,
        };
        self.motion.tick(&mut env)
    }

    /// Waits (real time) for the planner pool to answer, and hands the answer over.
    fn await_answer(&mut self) -> BotCmd {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(cmd) = self.rx.try_recv() {
                return cmd;
            }
            assert!(Instant::now() < until, "the planner never answered");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Ticks until the trip is over; returns the positions after each tick and the status.
    fn run(&mut self, dt: u64, max_ticks: usize) -> (Vec<[f32; 3]>, MotionStatus) {
        let mut seen = vec![self.motion.pos()];
        let mut st = self.motion.status();
        for _ in 0..max_ticks {
            st = self.tick(dt);
            seen.push(self.motion.pos());
            if st != MotionStatus::Moving {
                break;
            }
        }
        (seen, st)
    }

    fn go(&mut self, x: f32, y: f32, radius: f32) {
        self.motion.go_to(x, y, radius, Pace::Run, self.now);
    }
}

fn flags(p: &Out) -> u8 {
    assert_eq!(p.opcode, op::CSMoveUnitPacket);
    p.body[8]
}

fn velocity(p: &Out) -> (f32, f32) {
    let vx = i16::from_le_bytes([p.body[20], p.body[21]]);
    let vy = i16::from_le_bytes([p.body[22], p.body[23]]);
    // (the 1.2 client reads wire * 60 / 32767 m/s)
    (
        f32::from(vx) * 60.0 / 32767.0,
        f32::from(vy) * 60.0 / 32767.0,
    )
}

fn path_len(points: &[[f32; 3]]) -> f32 {
    points
        .windows(2)
        .map(|w| ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt())
        .sum()
}

#[test]
fn a_short_trip_goes_around_the_wall_and_arrives() {
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    rig.go(150.0, 40.0, 0.0);
    let (seen, st) = rig.run(250, 400);
    assert_eq!(st, MotionStatus::Arrived);
    let end = rig.motion.pos();
    assert!(
        (end[0] - 150.0).abs() < 0.05 && (end[1] - 40.0).abs() < 0.05,
        "{end:?}"
    );
    // never inside the wall, never a jump
    let mut prev = [50.0, 40.0, FLOOR_Z];
    for p in &seen {
        assert!(!in_detour_wall(*p), "{p:?} is in the wall");
        let step = ((p[0] - prev[0]).powi(2) + (p[1] - prev[1]).powi(2)).sqrt();
        // (a quarter second at run speed; the lateral habit adds a little to the way)
        assert!(step <= 5.4 * 0.25 * 1.01, "jump of {step} m at {p:?}");
        assert!((p[2] - FLOOR_Z).abs() < 0.01);
        prev = *p;
    }
    // it did go up to the gap (y >= 80) and the way is longer than the straight line, not absurd
    assert!(seen.iter().any(|p| p[1] >= 79.0));
    let walked = path_len(&seen);
    assert!((125.0..175.0).contains(&walked), "walked {walked} m");
    // one stopped update at the end, moving updates before it, all of them a tick apart
    assert_eq!(flags(rig.out.last().unwrap()), 0x04);
    assert_eq!(rig.out.iter().filter(|p| flags(p) == 0x04).count(), 1);
    let moving = rig.out.iter().filter(|p| flags(p) == 0x02).count();
    assert!(moving as f32 >= walked / 1.35 - 8.0, "{moving} updates");
    // counters: one inline search, one found, one arrived trip, a detour of 1.2 to 1.7
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.queries_inline, s.queries_pooled, s.found), (1, 0, 1));
    assert_eq!((s.trips, s.trips_arrived, s.legacy_fallbacks), (1, 1, 0));
    assert!((1.2..1.75).contains(&s.detour_ratio), "{}", s.detour_ratio);
}

#[test]
fn the_same_trip_without_the_mesh_goes_through_the_wall() {
    let mut rig = Rig::new(
        detour_mesh(),
        [50.0, 40.0, FLOOR_Z],
        Opts {
            enabled: false,
            ..Opts::default()
        },
    );
    rig.go(150.0, 40.0, 0.0);
    let (seen, st) = rig.run(250, 400);
    assert_eq!(st, MotionStatus::Arrived);
    assert!(seen.iter().any(|p| in_detour_wall(*p)));
    assert!((path_len(&seen) - 100.0).abs() < 0.5);
    // still counted, so the two modes can be compared
    let s = rig.metrics.snapshot(false);
    assert_eq!((s.trips, s.trips_arrived), (1, 1));
    assert_eq!(s.queries_inline + s.queries_pooled, 0);
    assert!((s.detour_ratio - 1.0).abs() < 0.01);
}

#[test]
fn the_pace_slows_down_over_the_last_metres_and_the_stop_is_exact() {
    let mut rig = Rig::new(
        testmesh::grid((0.0, 0.0), (40, 20), CELL, |_, _| false),
        [10.0, 50.0, FLOOR_Z],
        Opts::default(),
    );
    rig.go(100.0, 50.0, 3.0);
    let (_, st) = rig.run(250, 400);
    assert_eq!(st, MotionStatus::Arrived);
    // on the circle of the stop radius around the goal (a little to the side of the line: the
    // bot's lateral habit has not faded out yet)
    let pos = rig.motion.pos();
    let r = ((pos[0] - 100.0).powi(2) + (pos[1] - 50.0).powi(2)).sqrt();
    assert!((r - 3.0).abs() < 0.05, "{pos:?} is {r} m from the goal");
    // the speed of the moving updates: run at the start, walk-ish at the end
    let speeds: Vec<f32> = rig
        .out
        .iter()
        .filter(|p| flags(p) == 0x02)
        .map(|p| {
            let v = velocity(p);
            (v.0 * v.0 + v.1 * v.1).sqrt()
        })
        .collect();
    assert!((speeds[0] - 5.4).abs() < 0.05, "{speeds:?}");
    let last = *speeds.last().unwrap();
    assert!((1.7..4.5).contains(&last), "{speeds:?}");
    assert!(speeds.windows(2).all(|w| w[1] <= w[0] + 0.01), "{speeds:?}");
    // a slower finish than the plain run: 87 m at 5.4 is 16.1 s, the ramp costs about half a second
    let ticks = rig.out.iter().filter(|p| flags(p) == 0x02).count() as f32 * 0.25;
    assert!(ticks > 16.3 && ticks < 18.5, "{ticks} s");
}

#[test]
fn a_walk_pace_trip_is_walked_all_the_way() {
    let mut rig = Rig::new(
        testmesh::grid((0.0, 0.0), (40, 20), CELL, |_, _| false),
        [10.0, 50.0, FLOOR_Z],
        Opts::default(),
    );
    rig.motion.go_to(40.0, 50.0, 0.0, Pace::Walk, rig.now);
    rig.run(250, 400);
    for p in rig.out.iter().filter(|p| flags(p) == 0x02) {
        let v = velocity(p);
        assert!(((v.0 * v.0 + v.1 * v.1).sqrt() - 1.8).abs() < 0.01);
    }
}

#[test]
fn a_long_trip_is_planned_on_the_pool_while_the_bot_waits() {
    let mut rig = Rig::new(
        long_mesh(),
        [20.0, 10.0, FLOOR_Z],
        Opts {
            pool: true,
            ..Opts::default()
        },
    );
    rig.go(580.0, 10.0, 0.0);
    // the bot stands, faced towards the goal (east: yaw -pi/2), until the answer is there
    rig.tick(250);
    assert_eq!(rig.motion.status(), MotionStatus::Moving);
    assert_eq!(rig.motion.pos(), [20.0, 10.0, FLOOR_Z]);
    assert!((rig.motion.yaw() + std::f32::consts::FRAC_PI_2).abs() < 0.01);
    assert!(rig.out.iter().all(|p| flags(p) == 0x04), "never walking");
    // the answer comes by command; the next tick walks
    let cmd = rig.await_answer();
    let BotCmd::PathReady { ticket, result } = cmd else {
        panic!("not a path answer");
    };
    assert!(matches!(*result, PathResult::Found(_)));
    rig.motion.on_path_ready(ticket, *result);
    let (seen, st) = rig.run(250, 1000);
    assert_eq!(st, MotionStatus::Arrived, "{:?}", rig.motion.pos());
    // the route is about 600 m: more than the 90 s of the straight line, and it still arrives
    assert!(
        seen.len() as u64 * 250 > TRIP_TIMEOUT_MS,
        "{} ticks",
        seen.len()
    );
    assert!(path_len(&seen) > 560.0);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.queries_inline, s.queries_pooled), (0, 1));
    assert_eq!((s.wait_p50_us > 0, s.compute_p50_us > 0), (true, true));
    assert_eq!(s.trips_arrived, 1);
}

#[test]
fn a_late_answer_makes_the_bot_walk_the_straight_line_and_is_then_dropped() {
    let mut rig = Rig::new(
        long_mesh(),
        [20.0, 10.0, FLOOR_Z],
        Opts {
            pool: true,
            ..Opts::default()
        },
    );
    rig.go(580.0, 10.0, 0.0);
    // (the answer waits in the channel: nobody hands it over)
    for _ in 0..6 {
        rig.tick(250);
    }
    assert_eq!(rig.motion.pos()[0], 20.0, "still waiting after 1.5 s");
    rig.tick(250);
    rig.tick(250);
    assert!(rig.motion.pos()[0] > 20.0, "walking the straight line now");
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.plan_timeouts, s.legacy_fallbacks), (1, 1));
    // the late answer finds no trip waiting for it
    let before = rig.motion.pos();
    let _ = rig.await_answer_into_motion();
    let s = rig.metrics.snapshot(true);
    assert_eq!(s.stale_results, 1);
    rig.tick(250);
    assert!(rig.motion.pos()[0] > before[0]);
    assert!(
        (rig.motion.pos()[1] - 10.0).abs() < 0.01,
        "still the straight line"
    );
}

impl Rig {
    fn await_answer_into_motion(&mut self) -> u32 {
        match self.await_answer() {
            BotCmd::PathReady { ticket, result } => {
                self.motion.on_path_ready(ticket, *result);
                ticket
            }
            _ => panic!("not a path answer"),
        }
    }
}

#[test]
fn an_answer_for_an_older_trip_is_dropped() {
    let mut rig = Rig::new(
        long_mesh(),
        [20.0, 10.0, FLOOR_Z],
        Opts {
            pool: true,
            ..Opts::default()
        },
    );
    rig.go(580.0, 10.0, 0.0);
    rig.tick(250);
    // the trip is replaced by another long one before the first answer arrives
    rig.go(500.0, 50.0, 0.0);
    rig.tick(250);
    let first = rig.await_answer();
    let second = rig.await_answer();
    let (
        BotCmd::PathReady {
            ticket: t1,
            result: r1,
        },
        BotCmd::PathReady {
            ticket: t2,
            result: r2,
        },
    ) = (first, second)
    else {
        panic!("not path answers");
    };
    assert_ne!(t1, t2);
    rig.motion.on_path_ready(t1, *r1);
    assert_eq!(rig.metrics.snapshot(true).stale_results, 1);
    assert_eq!(rig.motion.pos()[0], 20.0, "still waiting for the second");
    rig.motion.on_path_ready(t2, *r2);
    rig.tick(250);
    assert!(rig.motion.pos()[0] > 20.0);
    assert_eq!(rig.metrics.snapshot(true).stale_results, 1);
}

#[test]
fn a_cancelled_trip_drops_its_answer() {
    let mut rig = Rig::new(
        long_mesh(),
        [20.0, 10.0, FLOOR_Z],
        Opts {
            pool: true,
            ..Opts::default()
        },
    );
    rig.go(580.0, 10.0, 0.0);
    rig.tick(250);
    rig.motion.stop();
    rig.await_answer_into_motion();
    assert_eq!(rig.metrics.snapshot(true).stale_results, 1);
    assert_eq!(rig.motion.status(), MotionStatus::Idle);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.trips, s.trips_cancelled), (1, 1));
}

#[test]
fn a_full_queue_means_the_straight_line_at_once() {
    let mut rig = Rig::new(long_mesh(), [20.0, 10.0, FLOOR_Z], Opts::default());
    // a pool that takes nothing
    let metrics = rig.metrics.clone();
    let slot = BotSlot::new(pb_web::BotSnapshot::default());
    let _rx = slot.attach();
    let world: Arc<dyn NavWorld> = FlatWorld::new(long_mesh());
    rig.motion.attach_nav(NavLink {
        enabled: true,
        world,
        planner: Some(Arc::new(Planner::start(1, 0, metrics.clone()))),
        reply: slot,
        metrics: metrics.clone(),
        main: "main_world".into(),
        seed: 7,
    });
    rig.go(580.0, 10.0, 0.0);
    rig.tick(250);
    assert!(rig.motion.pos()[0] > 20.0, "walks straight at once");
    let s = metrics.snapshot(true);
    assert_eq!((s.queue_full, s.legacy_fallbacks), (1, 1));
}

#[test]
fn an_unreachable_goal_and_one_off_the_mesh_are_the_straight_line() {
    // an island of 2 x 2 cells inside a ring of walls
    let island = testmesh::grid((0.0, 0.0), (20, 20), CELL, |cx, cy| {
        (13..=16).contains(&cx)
            && (13..=16).contains(&cy)
            && !((14..=15).contains(&cx) && (14..=15).contains(&cy))
    });
    let mut rig = Rig::new(island, [10.0, 10.0, FLOOR_Z], Opts::default());
    rig.go(77.0, 77.0, 0.0);
    let (_, st) = rig.run(250, 400);
    assert_eq!(st, MotionStatus::Arrived);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.unreachable, s.legacy_fallbacks, s.found), (1, 1, 0));
    assert!((rig.motion.pos()[0] - 77.0).abs() < 0.05);

    // 300 m beyond the edge of the mesh
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    rig.go(50.0, 340.0, 0.0);
    let (_, st) = rig.run(250, 400);
    assert_eq!(st, MotionStatus::Arrived);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.no_mesh, s.legacy_fallbacks), (1, 1));
}

#[test]
fn a_goal_a_few_metres_off_the_mesh_is_snapped_onto_it() {
    // the mesh ends at x = 200; the goal is on a porch 15 m beyond
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    rig.go(215.0, 40.0, 0.0);
    let (seen, st) = rig.run(250, 400);
    assert_eq!(st, MotionStatus::Arrived);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.found, s.no_mesh, s.legacy_fallbacks), (1, 0, 0));
    // (the first try found nothing within 8 m, the second snapped from 24 m)
    assert!(seen.iter().all(|p| !in_detour_wall(*p)));
    assert!((rig.motion.pos()[0] - 215.0).abs() < 0.05);
}

#[test]
fn a_partial_route_is_continued_from_its_end() {
    let mut rig = Rig::new(
        long_mesh(),
        [20.0, 10.0, FLOOR_Z],
        Opts {
            pool: true,
            ..Opts::default()
        },
    );
    rig.go(580.0, 10.0, 0.0);
    rig.tick(250);
    // the first answer is cut short, as a search that hit its cap would give it
    let BotCmd::PathReady { ticket, result } = rig.await_answer() else {
        panic!()
    };
    let PathResult::Found(full) = *result else {
        panic!("found")
    };
    let cut = full.points.len() / 2;
    assert!(cut >= 2, "{:?}", full.points);
    let points = full.points[..cut].to_vec();
    let partial = Path {
        length: points.windows(2).map(|w| w[0].dist(w[1])).sum(),
        indoor: full.indoor[..cut - 1].to_vec(),
        points,
        ..full.clone()
    };
    let end = partial.points[cut - 1];
    rig.motion
        .on_path_ready(ticket, PathResult::Partial(partial));
    // the search for the rest starts at once, from the end of the partial route
    let BotCmd::PathReady {
        ticket: t2,
        result: r2,
    } = rig.await_answer()
    else {
        panic!()
    };
    assert_ne!(ticket, t2);
    let PathResult::Found(rest) = &*r2 else {
        panic!("the continuation is found")
    };
    assert!(
        rest.points[0].dist2d(end) < 0.05,
        "{:?} vs {end:?}",
        rest.points[0]
    );
    // it arrives before the bot has walked the first part: no stop in between
    rig.motion.on_path_ready(t2, *r2);
    let mut stopped_early = false;
    let mut st = MotionStatus::Moving;
    for _ in 0..2000 {
        let before = rig.motion.pos();
        st = rig.tick(250);
        if st != MotionStatus::Moving {
            break;
        }
        if rig.motion.pos() == before {
            stopped_early = true;
        }
    }
    assert_eq!(st, MotionStatus::Arrived);
    assert!(!stopped_early);
    let s = rig.metrics.snapshot(true);
    assert_eq!(
        (s.partial, s.replans),
        (0, 1),
        "(the cut was made by the test)"
    );
    assert_eq!(s.trips_arrived, 1);
}

#[test]
fn a_partial_route_waits_a_moment_for_its_continuation_and_then_gives_up() {
    let mut rig = Rig::new(
        long_mesh(),
        [20.0, 10.0, FLOOR_Z],
        Opts {
            pool: true,
            ..Opts::default()
        },
    );
    rig.go(580.0, 10.0, 0.0);
    rig.tick(250);
    let BotCmd::PathReady { ticket, result } = rig.await_answer() else {
        panic!()
    };
    let PathResult::Found(full) = *result else {
        panic!()
    };
    // a short stretch ending at (60, 10); the continuation's answer is never handed over
    let cut = full.points.iter().position(|p| p.x > 60.0).unwrap();
    let mut points = full.points[..cut].to_vec();
    points.push(aa_world::geom::Vec3::new(60.0, 10.0, FLOOR_Z));
    let partial = Path {
        indoor: vec![false; points.len() - 1],
        length: points.windows(2).map(|w| w[0].dist(w[1])).sum(),
        points,
        ..full
    };
    rig.motion
        .on_path_ready(ticket, PathResult::Partial(partial));
    let mut waited = false;
    for _ in 0..80 {
        let before = rig.motion.pos();
        rig.tick(250);
        if rig.motion.pos() == before && rig.motion.pos()[0] > 59.0 {
            waited = true;
        }
    }
    assert!(waited, "stood at the end of the stretch");
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.plan_timeouts, s.legacy_fallbacks), (1, 1));
    // and then walked on, straight, east
    assert!(rig.motion.pos()[0] > 100.0);
}

#[test]
fn a_bot_that_cannot_move_is_replanned_twice_then_side_steps_and_gives_up() {
    let mut rig = Rig::new(
        detour_mesh(),
        [50.0, 40.0, FLOOR_Z],
        Opts {
            run_speed: 0.05,
            ..Opts::default()
        },
    );
    rig.go(150.0, 40.0, 0.0);
    let mut seconds = 0;
    let st = loop {
        let st = rig.tick(1000);
        seconds += 1;
        if st != MotionStatus::Moving {
            break st;
        }
        assert!(seconds < 60);
    };
    assert_eq!(st, MotionStatus::Failed(MoveFailure::Stuck));
    let s = rig.metrics.snapshot(true);
    assert_eq!(s.stuck_replans, 2, "{s:?}");
    assert_eq!(s.stuck_events, 5);
    assert_eq!((s.legacy_fallbacks, s.trips_failed), (1, 1));
    // the failure ends with a stopped update
    assert_eq!(flags(rig.out.last().unwrap()), 0x04);
}

#[test]
fn a_replan_avoids_the_spot_the_bot_got_stuck_on() {
    // an open field: the route is a straight line; after the stuck spot is made expensive the
    // second route is a different one
    let mut rig = Rig::new(
        testmesh::grid((0.0, 0.0), (40, 20), CELL, |_, _| false),
        [10.0, 50.0, FLOOR_Z],
        Opts {
            run_speed: 0.05,
            ..Opts::default()
        },
    );
    rig.go(190.0, 50.0, 0.0);
    rig.tick(1000);
    let mut first = Vec::new();
    rig.motion.route_points(&mut first, 32);
    assert_eq!(
        first.len(),
        2,
        "a straight line in an open field: {first:?}"
    );
    for _ in 0..3 {
        rig.tick(1000);
    }
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.stuck_replans, s.queries_inline), (1, 2), "{s:?}");
    // the new route bends around the nodes ahead of the bot
    let mut second = Vec::new();
    rig.motion.route_points(&mut second, 32);
    assert!(second.len() > 2, "{second:?}");
    assert!(
        second.iter().any(|p| (p[1] - 50.0).abs() > 1.5),
        "{second:?}"
    );
}

#[test]
fn pin_reports_the_heading_of_the_route_not_the_direction_of_the_goal() {
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    rig.go(150.0, 40.0, 0.0);
    for _ in 0..4 {
        rig.tick_gap(250, 1000);
    }
    // the goal is east; the bot is on its way north-east to the gap
    let pin = rig
        .motion
        .pin(rig.now, false)
        .expect("the server is behind");
    assert_eq!(flags(&pin), 0x02);
    let v = velocity(&pin);
    assert!(v.1 > 1.0, "heading north: {v:?}");
    assert!(((v.0 * v.0 + v.1 * v.1).sqrt() - 5.4).abs() < 0.1);
    // while the bot waits for a search it stands, and a pin says so
    let mut rig = Rig::new(
        long_mesh(),
        [20.0, 10.0, FLOOR_Z],
        Opts {
            pool: true,
            ..Opts::default()
        },
    );
    rig.go(580.0, 10.0, 0.0);
    rig.tick(250);
    assert!(
        rig.motion.pin(rig.now, false).is_none(),
        "standing where the server knows it"
    );
}

#[test]
fn the_route_is_published_for_the_map_and_stays_small() {
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    let mut pts = Vec::new();
    rig.motion.route_points(&mut pts, 32);
    assert!(pts.is_empty(), "nothing while idle");
    rig.go(150.0, 40.0, 0.0);
    rig.tick(250);
    rig.motion.route_points(&mut pts, 32);
    assert!(pts.len() >= 3 && pts.len() <= 32, "{pts:?}");
    let pos = rig.motion.pos();
    assert!((pts[0][0] - pos[0]).abs() < 0.01 && (pts[0][1] - pos[1]).abs() < 0.01);
    assert_eq!(*pts.last().unwrap(), [150.0, 40.0]);
    // the line goes through the gap, not the wall
    assert!(pts.iter().any(|p| p[1] > 79.0));
    // the straight line shows as two points
    let mut rig = Rig::new(
        detour_mesh(),
        [50.0, 40.0, FLOOR_Z],
        Opts {
            enabled: false,
            ..Opts::default()
        },
    );
    rig.go(150.0, 40.0, 0.0);
    rig.tick(250);
    let mut pts = Vec::new();
    rig.motion.route_points(&mut pts, 32);
    assert_eq!(pts.len(), 2);
    assert_eq!(pts[1], [150.0, 40.0]);
}

#[test]
fn an_instance_world_uses_its_own_mesh_and_the_height_of_the_route() {
    // no terrain heights inside an instance: the route gives the height
    let mut rig = Rig::new(
        detour_mesh(),
        [50.0, 40.0, FLOOR_Z],
        Opts {
            terrain: false,
            ..Opts::default()
        },
    );
    let mut place = Place::default();
    place.on_load(5, 90, 10);
    place.resolve_with(|_| Some("instance_dungeon".into()));
    rig.motion.sync_place(&place);
    rig.go(150.0, 40.0, 0.0);
    let (seen, st) = rig.run(250, 400);
    assert_eq!(st, MotionStatus::Arrived);
    assert!(seen.iter().all(|p| !in_detour_wall(*p)));
    assert!(seen.iter().all(|p| (p[2] - FLOOR_Z).abs() < 0.01));
    assert_eq!(rig.metrics.snapshot(true).found, 1);

    // an instance whose world is not known walks straight lines
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    let mut place = Place::default();
    place.on_load(5, 91, 10);
    place.resolve_with(|_| None);
    rig.motion.sync_place(&place);
    rig.go(150.0, 40.0, 0.0);
    let (seen, _) = rig.run(250, 400);
    assert!(seen.iter().any(|p| in_detour_wall(*p)));
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.found, s.legacy_fallbacks), (0, 1));
    // back in the main world the mesh is used again
    rig.motion.sync_place(&Place::default());
    rig.motion.teleported([50.0, 40.0, FLOOR_Z]);
    rig.go(150.0, 40.0, 0.0);
    let (seen, _) = rig.run(250, 400);
    assert!(seen.iter().all(|p| !in_detour_wall(*p)));
}

#[test]
fn the_leg_length_follows_the_mesh() {
    let rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    assert_eq!(rig.motion.leg_m(), super::trip::LEG_TARGET_M);
    let rig = Rig::new(
        detour_mesh(),
        [50.0, 40.0, FLOOR_Z],
        Opts {
            enabled: false,
            ..Opts::default()
        },
    );
    assert_eq!(rig.motion.leg_m(), super::trip::LEGACY_LEG_M);
    assert_eq!(
        Motion::new(MotionCfg::default()).leg_m(),
        super::trip::LEGACY_LEG_M
    );
}

#[test]
fn a_trip_that_started_in_a_teleport_or_a_new_world_is_forgotten() {
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    rig.go(150.0, 40.0, 0.0);
    rig.tick(250);
    rig.motion.teleported([60.0, 60.0, FLOOR_Z]);
    assert_eq!(rig.motion.status(), MotionStatus::Idle);
    let n = rig.out.len();
    rig.tick(250);
    assert_eq!(rig.out.len(), n);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.trips, s.trips_cancelled), (1, 1));
}

#[test]
fn bots_of_different_seeds_keep_different_distances_from_the_line() {
    // on a long open street two bots never walk the same line
    let mesh = || testmesh::grid((0.0, 0.0), (120, 12), CELL, |_, _| false);
    let walk = |seed: u32| {
        let mut rig = Rig::new(mesh(), [20.0, 30.0, FLOOR_Z], Opts::default());
        // (a different habit comes from a different seed)
        let slot = BotSlot::new(pb_web::BotSnapshot::default());
        let _rx = slot.attach();
        let world: Arc<dyn NavWorld> = FlatWorld::new(mesh());
        rig.motion.attach_nav(NavLink {
            enabled: true,
            world,
            planner: None,
            reply: slot,
            metrics: rig.metrics.clone(),
            main: "main_world".into(),
            seed,
        });
        rig.go(500.0, 30.0, 0.0);
        let (seen, st) = rig.run(250, 1000);
        assert_eq!(st, MotionStatus::Arrived);
        seen.iter()
            .map(|p| p[1] - 30.0)
            .fold(0.0f32, |a, b| if b.abs() > a.abs() { b } else { a })
    };
    let offsets: Vec<f32> = (1..=6).map(walk).collect();
    // each walks beside the line by 0.35 to 1.1 m (and returns to it), the sides are mixed
    for o in &offsets {
        assert!((0.3..=1.15).contains(&o.abs()), "{offsets:?}");
    }
    assert!(
        offsets.iter().any(|o| *o > 0.0) && offsets.iter().any(|o| *o < 0.0),
        "{offsets:?}"
    );
    // and the same bot always walks the same way
    assert_eq!(walk(3), walk(3));
}

#[test]
fn a_goal_beyond_one_search_is_walked_in_pieces() {
    // 2.5 km of open street; the goal is 2.4 km away, more than one search is asked for
    let mesh = testmesh::grid((0.0, 0.0), (500, 6), CELL, |_, _| false);
    let mut rig = Rig::new(
        mesh,
        [10.0, 15.0, FLOOR_Z],
        Opts {
            pool: true,
            ..Opts::default()
        },
    );
    rig.go(2410.0, 15.0, 0.0);
    rig.tick(250);
    // the first piece ends 1500 m from the start
    let BotCmd::PathReady { ticket, result } = rig.await_answer() else {
        panic!()
    };
    let PathResult::Found(first) = &*result else {
        panic!("found")
    };
    assert!((first.points.last().unwrap().x - 1510.0).abs() < 1.0);
    rig.motion.on_path_ready(ticket, *result);
    // the search for the rest has started already
    let BotCmd::PathReady { ticket, result } = rig.await_answer() else {
        panic!()
    };
    let PathResult::Found(rest) = &*result else {
        panic!("found")
    };
    assert!((rest.points[0].x - 1510.0).abs() < 1.0);
    assert!((rest.points.last().unwrap().x - 2410.0).abs() < 0.01);
    rig.motion.on_path_ready(ticket, *result);
    let (_, st) = rig.run(250, 3000);
    assert_eq!(st, MotionStatus::Arrived);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.queries_pooled, s.replans, s.legacy_fallbacks), (2, 1, 0));
    // 2.4 km takes seven and a half minutes: the trip limit grew with the route
    assert!(rig.now - T0 > 4 * TRIP_TIMEOUT_MS, "{} ms", rig.now - T0);
}

#[test]
fn a_planner_pool_that_is_dropped_right_after_it_starts_stops() {
    // the stop flag races with a worker that is about to wait: many short-lived pools show a
    // wake-up that is lost
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..400 {
            drop(Planner::start(2, 8, Arc::new(NavMetrics::default())));
        }
        let _ = done.send(());
    });
    assert!(
        finished.recv_timeout(Duration::from_secs(30)).is_ok(),
        "a planner pool did not stop"
    );
}

#[test]
fn many_bots_share_a_small_pool_without_losing_an_answer() {
    let metrics = Arc::new(NavMetrics::default());
    let planner = Planner::start(2, 64, metrics.clone());
    let world: Arc<dyn NavWorld> = FlatWorld::new(long_mesh());
    let mesh = world.mesh("main_world").unwrap();
    let slots: Vec<_> = (0..40)
        .map(|_| {
            let slot = BotSlot::new(pb_web::BotSnapshot::default());
            let rx = slot.attach();
            (slot, rx)
        })
        .collect();
    for (i, (slot, _)) in slots.iter().enumerate() {
        let job = super::Job {
            ticket: i as u32 + 1,
            mesh: mesh.clone(),
            world: world.clone(),
            spec: super::PlanSpec {
                from: [20.0, 10.0, FLOOR_Z],
                to: [580.0, 10.0 + (i % 40) as f32, FLOOR_Z],
                max_expansions: 60_000,
                penalties: Vec::new(),
                terrain: true,
            },
            reply: slot.clone(),
            submitted: Instant::now(),
        };
        assert!(planner.submit(job).is_ok());
    }
    for (i, (_, mut rx)) in slots.into_iter().enumerate() {
        let until = Instant::now() + Duration::from_secs(20);
        let cmd = loop {
            if let Ok(c) = rx.try_recv() {
                break c;
            }
            assert!(Instant::now() < until, "bot {i} got no answer");
            std::thread::sleep(Duration::from_millis(1));
        };
        let BotCmd::PathReady { ticket, result } = cmd else {
            panic!()
        };
        assert_eq!(ticket, i as u32 + 1);
        assert!(matches!(*result, PathResult::Found(_)));
    }
    let s = metrics.snapshot(true);
    assert_eq!((s.queries_pooled, s.found), (40, 40));
    assert!(s.queue_max >= 1 && s.queue_max <= 40);
    assert_eq!(planner.depth(), 0);
    assert_eq!(planner.threads(), 2);
    // dropping the pool stops its threads
    drop(planner);
}

#[test]
fn asking_for_the_same_goal_again_keeps_the_route() {
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    rig.go(150.0, 40.0, 0.0);
    for _ in 0..8 {
        rig.tick(250);
        // a goal that asks every tick (the chase of a monster that stands still)
        rig.go(150.2, 40.0, 0.0);
    }
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.queries_inline, s.trips, s.trips_cancelled), (1, 1, 0));
    let (seen, st) = rig.run(250, 400);
    assert_eq!(st, MotionStatus::Arrived);
    assert!(seen.iter().all(|p| !in_detour_wall(*p)));
    // another goal is another trip
    rig.go(60.0, 40.0, 0.0);
    rig.go(120.0, 40.0, 0.0);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.trips, s.trips_cancelled), (3, 1));
}

#[test]
fn a_goal_that_stops_the_walk_in_range_counts_it_as_arrived() {
    let mut rig = Rig::new(
        testmesh::grid((0.0, 0.0), (40, 20), CELL, |_, _| false),
        [10.0, 50.0, FLOOR_Z],
        Opts::default(),
    );
    rig.go(100.0, 50.0, 2.0);
    // (the goal is in range when the stop radius and a few metres are left)
    while rig.motion.remaining().unwrap() > 6.0 {
        assert_eq!(rig.tick(250), MotionStatus::Moving);
    }
    rig.motion.stop();
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.trips, s.trips_arrived, s.trips_cancelled), (1, 1, 0));
    // far from the goal the same stop is a cancelled trip
    rig.go(10.0, 50.0, 2.0);
    rig.tick(250);
    rig.motion.stop();
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.trips, s.trips_arrived, s.trips_cancelled), (2, 1, 1));
}

#[test]
fn a_chase_is_no_trip_in_the_statistics_but_cuts_a_goals_walk_off() {
    let mut rig = Rig::new(detour_mesh(), [50.0, 40.0, FLOOR_Z], Opts::default());
    // the chase of a monster that keeps moving: a new goal every tick, and then the stop
    for i in 0..6 {
        let now = rig.now;
        rig.motion
            .chase_to(150.0 + i as f32 * 3.0, 40.0, 2.0, Pace::Run, now);
        assert_eq!(rig.tick(250), MotionStatus::Moving);
    }
    rig.motion.stop();
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.trips, s.trips_arrived, s.trips_cancelled), (0, 0, 0));
    // the walk of a goal that a fight cuts off is a cancelled trip, the chase after it is none
    rig.go(190.0, 40.0, 0.0);
    rig.tick(250);
    let now = rig.now;
    rig.motion.chase_to(60.0, 40.0, 2.0, Pace::Run, now);
    let s = rig.metrics.snapshot(true);
    assert_eq!((s.trips, s.trips_arrived, s.trips_cancelled), (1, 0, 1));
    // (and the chase is walked like any trip)
    assert_eq!(rig.tick(250), MotionStatus::Moving);
}
