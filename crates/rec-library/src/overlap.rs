//! Overlapping clips. Two clips overlap exactly when their ranges in the
//! same game session intersect, so the same moment saved twice (or a clip
//! and its trim) is found without looking at a single frame.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::clip::{Clip, round3};

/// Intersections shorter than this are clips saved back to back.
pub const MIN_OVERLAP: f64 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// Both clips cover the same span.
    Same,
    /// This clip covers all of the other one.
    Contains,
    /// The other clip covers all of this one.
    Inside,
    Partial,
}

impl Relation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Contains => "contains",
            Self::Inside => "inside",
            Self::Partial => "partial",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Overlap {
    /// Index of the other clip.
    pub other: usize,
    /// Seconds both clips cover.
    pub seconds: f64,
    /// The other clip's time for this clip's time `t` is `t + offset`.
    pub offset: f64,
    /// This clip's time span shared with the other: `[from, to]`.
    pub shared: (f64, f64),
    pub relation: Relation,
}

/// A set of clips connected by overlaps.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// `g1`, `g2`, … in order of first clip.
    pub id: String,
    pub session: String,
    /// Indices into the clip list, by start in the session.
    pub clips: Vec<usize>,
    /// Session seconds the group spans.
    pub from: f64,
    pub to: f64,
}

/// For each clip, the clips it overlaps, by start.
pub fn overlaps(clips: &[Clip]) -> Vec<Vec<Overlap>> {
    let mut result = vec![Vec::new(); clips.len()];
    for members in by_session(clips).into_values() {
        for (n, &(i, a_from, a_to)) in members.iter().enumerate() {
            for &(j, b_from, b_to) in &members[n + 1..] {
                if b_from >= a_to {
                    break;
                }
                let (from, to) = (a_from.max(b_from), a_to.min(b_to));
                if to - from < MIN_OVERLAP {
                    continue;
                }
                let same =
                    (a_from - b_from).abs() < MIN_OVERLAP && (a_to - b_to).abs() < MIN_OVERLAP;
                let a_holds_b = a_from <= b_from + MIN_OVERLAP && a_to + MIN_OVERLAP >= b_to;
                let b_holds_a = b_from <= a_from + MIN_OVERLAP && b_to + MIN_OVERLAP >= a_to;
                let (ra, rb) = if same {
                    (Relation::Same, Relation::Same)
                } else if a_holds_b {
                    (Relation::Contains, Relation::Inside)
                } else if b_holds_a {
                    (Relation::Inside, Relation::Contains)
                } else {
                    (Relation::Partial, Relation::Partial)
                };
                let seconds = round3(to - from);
                result[i].push(Overlap {
                    other: j,
                    seconds,
                    offset: round3(a_from - b_from),
                    shared: (round3(from - a_from), round3(to - a_from)),
                    relation: ra,
                });
                result[j].push(Overlap {
                    other: i,
                    seconds,
                    offset: round3(b_from - a_from),
                    shared: (round3(from - b_from), round3(to - b_from)),
                    relation: rb,
                });
            }
        }
    }
    for list in &mut result {
        list.sort_by(|a, b| a.shared.0.total_cmp(&b.shared.0));
    }
    result
}

/// Groups of two or more clips joined by overlaps. Each clip is in at most
/// one group.
pub fn groups(clips: &[Clip]) -> Vec<Group> {
    let mut groups = Vec::new();
    for (session, members) in by_session(clips) {
        // Members are sorted by start, so a sweep finds connected runs.
        let mut current: Option<Group> = None;
        for (i, from, to) in members {
            match &mut current {
                Some(g) if from < g.to - MIN_OVERLAP => {
                    g.clips.push(i);
                    g.to = g.to.max(to);
                }
                _ => {
                    if let Some(g) = current.take().filter(|g| g.clips.len() > 1) {
                        groups.push(g);
                    }
                    current = Some(Group {
                        id: String::new(),
                        session: session.to_owned(),
                        clips: vec![i],
                        from,
                        to,
                    });
                }
            }
        }
        if let Some(g) = current.filter(|g| g.clips.len() > 1) {
            groups.push(g);
        }
    }
    groups.sort_by(|a, b| a.clips[0].cmp(&b.clips[0]));
    for (n, g) in groups.iter_mut().enumerate() {
        g.id = format!("g{}", n + 1);
    }
    groups
}

/// The group id of each clip, if any.
pub fn group_of(groups: &[Group], count: usize) -> Vec<Option<String>> {
    let mut of = vec![None; count];
    for g in groups {
        for &i in &g.clips {
            of[i] = Some(g.id.clone());
        }
    }
    of
}

/// JSON for one clip's overlaps, as `rec digest` and `rec mcp` give them.
pub fn to_json(clips: &[Clip], list: &[Overlap]) -> Value {
    list.iter()
        .map(|o| {
            let other = &clips[o.other];
            json!({
                "id": other.id,
                "file": other.file,
                "relation": o.relation.name(),
                "seconds": o.seconds,
                "shared": [o.shared.0, o.shared.1],
                "offset": o.offset,
            })
        })
        .collect()
}

/// `(index, from, to)` of each clip with a session range, by session and
/// start.
fn by_session(clips: &[Clip]) -> BTreeMap<&str, Vec<(usize, f64, f64)>> {
    let mut sessions: BTreeMap<&str, Vec<(usize, f64, f64)>> = BTreeMap::new();
    for (i, clip) in clips.iter().enumerate() {
        if let Some((session, from, to)) = clip.session_range() {
            sessions.entry(session).or_default().push((i, from, to));
        }
    }
    for members in sessions.values_mut() {
        members.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    }
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::tests::sample;

    fn clip(id: &str, session: &str, from: f64, duration: f64) -> Clip {
        let mut v = sample();
        v["clip"]["id"] = id.into();
        v["clip"]["duration"] = duration.into();
        v["time"]["session"] = session.into();
        v["time"]["session_offset"] = from.into();
        Clip::from_value(v).unwrap()
    }

    #[test]
    fn finds_overlaps_and_relations() {
        let clips = vec![
            clip("A", "s1", 100.0, 30.0), // 100–130
            clip("B", "s1", 120.0, 30.0), // 120–150, partial with A
            clip("C", "s1", 125.0, 5.0),  // 125–130, inside A and B
            clip("D", "s1", 150.0, 10.0), // 150–160, touches B only
            clip("E", "s2", 100.0, 30.0), // another session
            clip("F", "s2", 100.0, 30.0), // same as E
        ];
        let all = overlaps(&clips);
        let a: Vec<(usize, Relation, f64)> = all[0]
            .iter()
            .map(|o| (o.other, o.relation, o.seconds))
            .collect();
        assert_eq!(
            a,
            vec![(1, Relation::Partial, 10.0), (2, Relation::Contains, 5.0)]
        );
        // B's t = A's t - 20: A's 25 s is B's 5 s.
        assert_eq!(all[0][0].offset, -20.0);
        assert_eq!(all[0][0].shared, (20.0, 30.0));
        assert_eq!(all[1][0].offset, 20.0);
        assert_eq!(all[1][0].shared, (0.0, 10.0));
        assert_eq!(
            all[2].iter().map(|o| o.relation).collect::<Vec<_>>(),
            vec![Relation::Inside; 2]
        );
        assert!(all[3].is_empty(), "back to back is not an overlap");
        assert_eq!(all[4][0].relation, Relation::Same);

        let groups = groups(&clips);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].id, "g1");
        assert_eq!(groups[0].clips, vec![0, 1, 2]);
        assert_eq!((groups[0].from, groups[0].to), (100.0, 150.0));
        assert_eq!(groups[1].clips, vec![4, 5]);
        let of = group_of(&groups, clips.len());
        assert_eq!(of[3], None);
        assert_eq!(of[5].as_deref(), Some("g2"));
        let json = to_json(&clips, &all[0]);
        assert_eq!(json[0]["id"], "B");
        assert_eq!(json[0]["relation"], "partial");
    }

    #[test]
    fn clips_without_a_session_are_left_out() {
        let mut v = sample();
        v["time"] = json!({});
        let clips = vec![Clip::from_value(v).unwrap(), clip("A", "s1", 0.0, 10.0)];
        assert!(overlaps(&clips).iter().all(Vec::is_empty));
        assert!(groups(&clips).is_empty());
    }
}
