//! Long-context routing policy: the request's class, the Fleet's tier, and
//! (from the heavy lane) the route policy label.
//!
//! Both the class and the tier come from the pool, which owns the size
//! estimate and the tier boundary: a request is heavy when its context
//! requirement exceeds the smallest declared capacity, and a Fleet is `Long`
//! when its declared capacity is above that. Placement never re-derives
//! either from token counts.

/// The capacity tier of the Fleet a `Placer` serves. Fixed at construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    Base,
    Long,
}

impl Tier {
    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::Base => "base",
            Tier::Long => "long",
        }
    }
}

/// A request's size class, from the pool's `PlacementContext.heavy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Short,
    Heavy,
}

impl Class {
    pub const fn of(heavy: bool) -> Self {
        if heavy {
            Class::Heavy
        } else {
            Class::Short
        }
    }

    /// A stable, content-free name for logs and metric tags.
    pub const fn as_str(self) -> &'static str {
        match self {
            Class::Short => "short",
            Class::Heavy => "heavy",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tier_and_class_has_a_static_tag() {
        assert_eq!(Tier::Base.as_str(), "base");
        assert_eq!(Tier::Long.as_str(), "long");
        assert_eq!(Class::Short.as_str(), "short");
        assert_eq!(Class::Heavy.as_str(), "heavy");
        assert_eq!(Class::of(true), Class::Heavy);
        assert_eq!(Class::of(false), Class::Short);
    }
}
