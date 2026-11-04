//! Profiles (`docs/multi-user.md`): a user has one or more profiles, each
//! with its own bookmark tree, for example "Private" and "Work". Tokens and
//! devices belong to one profile; profiles share nothing.

use crate::auth::random_id;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_PROFILES_PER_USER: usize = 20;
pub const DEFAULT_NAME: &str = "Default";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub user_id: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct Profiles {
    by_id: BTreeMap<String, Profile>,
}

/// The id of the profile that an older state file's user gets: `u_x` -> `p_x`.
pub fn default_id_for(user_id: &str) -> String {
    format!("p_{}", user_id.strip_prefix("u_").unwrap_or(user_id))
}

pub fn check_name(name: &str) -> Result<(), String> {
    let n = name.chars().count();
    if !(1..=48).contains(&n) || name.chars().any(char::is_control) {
        return Err("profile name must be 1 to 48 characters without control characters".into());
    }
    Ok(())
}

impl Profiles {
    pub fn from_records(records: Vec<Profile>) -> Result<Self, String> {
        let mut p = Profiles::default();
        for r in records {
            check_name(&r.name)?;
            if p.by_id.values().filter(|x| x.user_id == r.user_id).count() >= MAX_PROFILES_PER_USER
            {
                return Err(format!(
                    "too many profiles for one user (max {MAX_PROFILES_PER_USER})"
                ));
            }
            if p.by_name(&r.user_id, &r.name).is_some() {
                return Err(format!("duplicate profile {} of one user", r.name));
            }
            p.by_id.insert(r.id.clone(), r);
        }
        Ok(p)
    }

    pub fn records(&self) -> Vec<Profile> {
        self.by_id.values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.by_id.get(id)
    }

    pub fn list(&self) -> impl Iterator<Item = &Profile> {
        self.by_id.values()
    }

    /// A user's profiles, by name.
    pub fn for_user(&self, user_id: &str) -> Vec<&Profile> {
        let mut v: Vec<_> = self
            .by_id
            .values()
            .filter(|p| p.user_id == user_id)
            .collect();
        v.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.id.cmp(&b.id))
        });
        v
    }

    /// Case-insensitive lookup within one user's profiles.
    pub fn by_name(&self, user_id: &str, name: &str) -> Option<&Profile> {
        let key = name.trim().to_lowercase();
        self.by_id
            .values()
            .find(|p| p.user_id == user_id && p.name.to_lowercase() == key)
    }

    pub fn create(&mut self, user_id: &str, name: &str) -> Result<Profile, String> {
        self.create_with_id(random_id("p_"), user_id, name)
    }

    pub fn create_with_id(
        &mut self,
        id: String,
        user_id: &str,
        name: &str,
    ) -> Result<Profile, String> {
        let name = name.trim();
        check_name(name)?;
        if self.by_name(user_id, name).is_some() {
            return Err(format!("you already have a profile called {name}"));
        }
        if self.for_user(user_id).len() >= MAX_PROFILES_PER_USER {
            return Err(format!("at most {MAX_PROFILES_PER_USER} profiles"));
        }
        let p = Profile {
            id,
            user_id: user_id.to_string(),
            name: name.to_string(),
        };
        self.by_id.insert(p.id.clone(), p.clone());
        Ok(p)
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<(), String> {
        let name = name.trim();
        check_name(name)?;
        let user_id = self.by_id.get(id).ok_or("no such profile")?.user_id.clone();
        if self
            .by_name(&user_id, name)
            .is_some_and(|other| other.id != id)
        {
            return Err(format!("you already have a profile called {name}"));
        }
        self.by_id.get_mut(id).expect("checked").name = name.to_string();
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.by_id.remove(id).is_some()
    }

    /// Drop every profile of a user; returns their ids.
    pub fn remove_user(&mut self, user_id: &str) -> Vec<String> {
        let ids: Vec<String> = self
            .by_id
            .values()
            .filter(|p| p.user_id == user_id)
            .map(|p| p.id.clone())
            .collect();
        for id in &ids {
            self.by_id.remove(id);
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_per_user_not_globally() {
        let mut p = Profiles::default();
        p.create("u_a", "Work").unwrap();
        assert!(p.create("u_a", " work ").is_err());
        p.create("u_b", "Work").unwrap();
        assert!(p.create("u_a", "").is_err());
        let id = p.create("u_a", "Private").unwrap().id;
        assert!(p.rename(&id, "WORK").is_err());
        p.rename(&id, "Home").unwrap();
        assert_eq!(p.by_name("u_a", "home").unwrap().id, id);
    }

    #[test]
    fn default_ids_follow_the_user() {
        assert_eq!(default_id_for("u_owner"), "p_owner");
        assert_eq!(default_id_for("u_ab12"), "p_ab12");
    }
}
