//! Wrappers for interacting with groups within Thorium with different backends
//! Currently only Redis is supported

use ldap3::{Scope, SearchEntry};
use std::collections::{HashMap, HashSet};
use tracing::{Level, event, instrument};

use super::db;
use super::db::groups::{MembersLists, RawGroupData};
use crate::models::groups::GroupUsers;
use crate::models::{
    Group, GroupAllowAction, GroupAllowed, GroupAllowedUpdate, GroupDetailsList, GroupList,
    GroupRequest, GroupStats, GroupUpdate, GroupUsersRequest, GroupUsersUpdate, ImageScaler, User,
};
use crate::utils::{ApiError, Shared, bounder};
use crate::{
    bad, conflict, deserialize_ext, deserialize_opt, ldap, not_found, unauthorized, unavailable,
    update, update_clear, update_opt_empty,
};

// Only build in when DB features are enabled
impl GroupRequest {
    /// validate all of our metagroup names
    ///
    /// This ensures that ldap injection is not possible through metagroup names.
    /// Only alphanumeric + '-' + '_' are allowed in metagroup names.
    pub fn validate_metagroups(&self) -> Result<(), ApiError> {
        // check all of the metagroups for all group roles
        bounder::ldap_metagroups(&self.owners.metagroups)?;
        bounder::ldap_metagroups(&self.managers.metagroups)?;
        bounder::ldap_metagroups(&self.users.metagroups)?;
        bounder::ldap_metagroups(&self.monitors.metagroups)?;
        Ok(())
    }

    /// Cast a `GroupRequest` to a Group
    ///
    /// # Arguments
    ///
    /// * `user` - The user creating this group
    /// * `shared` - Shared objects in Thorium
    pub async fn cast(mut self, user: &User, shared: &Shared) -> Result<Group, ApiError> {
        // bounds check string and ensure its alphanumeric and lowercase
        bounder::string_lower(&self.name, "group['name']", 1, 50)?;
        // bounds check our metagroups
        self.validate_metagroups()?;
        // make sure all users exist
        User::exists_many(&self.owners.direct, shared).await?;
        User::exists_many(&self.managers.direct, shared).await?;
        User::exists_many(&self.users.direct, shared).await?;
        User::exists_many(&self.monitors.direct, shared).await?;
        // get a list of analysts in Thorium
        let analysts = db::users::get_analysts(shared).await?;
        // inject creator as an owner if he is not already in there
        if !self.owners.direct.contains(&user.username) {
            self.owners.direct.insert(user.username.clone());
        }
        // cast our role requests
        let owners = GroupUsers::from(self.owners);
        let managers = GroupUsers::from(self.managers);
        let users = GroupUsers::from(self.users);
        let monitors = GroupUsers::from(self.monitors);
        // cast to group object
        let mut cast = Group {
            name: self.name,
            owners,
            managers,
            analysts,
            users,
            monitors,
            description: self.description,
            allowed: self.allowed,
        };
        // fix this groups roles if its needed
        cast.fix();
        Ok(cast)
    }
}

impl From<GroupUsersRequest> for GroupUsers {
    /// Cast a group users request to a group users
    ///
    /// # Arguments
    ///
    /// * `req` - The group users request to cast
    fn from(req: GroupUsersRequest) -> Self {
        GroupUsers {
            combined: req.direct.clone(),
            direct: req.direct,
            metagroups: req.metagroups,
        }
    }
}

impl GroupUsersUpdate {
    /// Apply this update to a groups direct users
    ///
    /// # Arguments
    ///
    /// * `group` - The group to apply updates too
    pub fn update_direct(
        &mut self,
        role: &mut GroupUsers,
        added: &mut HashSet<String>,
        removed: &mut HashSet<String>,
        valid: &HashSet<String>,
    ) -> Result<(), ApiError> {
        // make sure that all new users are valid
        if let Some(user) = added.iter().find(|user| !valid.contains(*user)) {
            return bad!(format!("{} is not a valid user.", user));
        }
        // remove any users that got removed and added to the same role
        self.direct_add
            .retain(|name| !self.direct_remove.contains(name));
        // add any new users to our our added set
        added.extend(self.direct_add.iter().cloned());
        // track the users we are removing
        removed.extend(self.direct_remove.iter().cloned());
        // remove any users that we are adding to a new role
        removed.retain(|name| !self.direct_add.contains(name));
        // update our direct users
        role.direct.extend(self.direct_add.drain());
        role.direct
            .retain(|name| !self.direct_remove.contains(name));
        // remove any removed users from our combined set
        role.combined.retain(|user| !removed.contains(user));
        // add these names to our combined set
        role.combined
            .extend(role.direct.iter().map(std::borrow::ToOwned::to_owned));
        Ok(())
    }

    /// Apply this update to a groups metagroups
    ///
    /// # Arguments
    ///
    /// * `group` - The group to apply updates too
    pub fn update_metagroups(&mut self, role: &mut GroupUsers) -> Result<(), ApiError> {
        // make sure all of our new metagroup names are valid and sanitized
        for metagroup in self.metagroups_add.drain() {
            // check this metagroup
            bounder::ldap_metagroup(&metagroup)?;
            // add this metagroup
            role.metagroups.insert(metagroup);
        }
        // remove any metagroups we no longer want
        role.metagroups
            .retain(|name| !self.metagroups_remove.contains(name));
        Ok(())
    }
}

impl GroupAllowedUpdate {
    /// Apply this update to our group
    ///
    /// # Arguments
    ///
    /// * `group` - The group to apply this update too
    pub fn update(&mut self, group: &mut Group) {
        // apply our updates
        update!(group.allowed.files, self.files);
        update!(group.allowed.repos, self.repos);
        update!(group.allowed.tags, self.tags);
        update!(group.allowed.images, self.images);
        update!(group.allowed.pipelines, self.pipelines);
        update!(group.allowed.reactions, self.reactions);
        update!(group.allowed.results, self.results);
        update!(group.allowed.comments, self.comments);
        update!(group.allowed.entities, self.entities);
        update!(group.allowed.associations, self.associations);
    }
}

impl GroupList {
    /// Creates a new group list object
    ///
    /// # Arguments
    ///
    /// * `cursor` - A cursor used page to the next list of groups
    /// * `names` - A list of group names
    pub(super) fn new(cursor: Option<usize>, names: Vec<String>) -> Self {
        GroupList { cursor, names }
    }
}

impl GroupDetailsList {
    /// Creates a new group details list object
    ///
    /// # Arguments
    ///
    /// * `cursor` - A cursor used page to the next list of groups
    /// * `details` - A list of group details
    pub(super) fn new(cursor: Option<usize>, details: Vec<Group>) -> Self {
        GroupDetailsList { cursor, details }
    }
}

/// Remove users or metagroups from any lower groups
///
/// # Arguments
///
/// * `target` - The users or metagroups to check
/// * `lower` - The lower roles to remove from
macro_rules! fix_roles {
    ($target:expr, $lower:expr) => {
        // crawl over each lower combined users and remove any duplicate users
        for lower in $lower.iter_mut() {
            lower
                .combined
                .retain(|name| !$target.combined.contains(name));
        }
        // crawl over each direct users and remove any duplicate users
        for lower in $lower.iter_mut() {
            lower.direct.retain(|name| !$target.direct.contains(name));
        }
        // crawl over each lower role and remove any duplicate metagroups
        for lower in $lower.iter_mut() {
            lower
                .metagroups
                .retain(|name| !$target.metagroups.contains(name));
        }
    };
}

impl Group {
    /// Creates a group object in the backend
    ///
    /// # Arguments
    ///
    /// * `user` - The user creating a group
    /// * `req` - The group request to use when creating this group
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Group::create", skip(user, shared), err(Debug))]
    pub async fn create(user: &User, req: GroupRequest, shared: &Shared) -> Result<Self, ApiError> {
        // ensure name is alphanumeric and lowercase
        bounder::string_lower(&req.name, "name", 1, 50)?;
        // error if ldap is not configured but ldap roles are
        if shared.config.thorium.auth.ldap.is_none()
            && (!req.owners.metagroups.is_empty()
                || !req.managers.metagroups.is_empty()
                || !req.users.metagroups.is_empty()
                || !req.monitors.metagroups.is_empty())
        {
            return unavailable!("Ldap is not configured".to_owned());
        }
        // check if this group is on the namespace blacklist
        if shared
            .config
            .thorium
            .namespace_blacklist
            .contains(&req.name)
        {
            // return an error if this group is in the blacklist
            return bad!(format!(
                "'{}' is a restricted name! Please choose a different name.",
                req.name
            ));
        }
        // check if this group already exists
        if db::groups::exists(std::slice::from_ref(&req.name), shared).await? {
            return unauthorized!();
        }
        // add group to backend
        let group = db::groups::create(user, req, shared).await?;
        Ok(group)
    }

    /// Fix a group so that any users or metagroups that are in multiple roles are valid.
    ///
    /// If there is overlap we will use the highest valid permission
    fn fix(&mut self) {
        // remove our owners from any lower roles
        fix_roles!(
            self.owners,
            &mut [&mut self.managers, &mut self.users, &mut self.monitors]
        );
        // remove our managers from any lower roles
        fix_roles!(self.managers, &mut [&mut self.users, &mut self.monitors]);
        // remove our users from any lower roles
        fix_roles!(self.users, &mut [&mut self.monitors]);
    }

    /// Lists all group names that you have permission to see
    ///
    /// # Arguments
    ///
    /// * `user` - The user listing groups
    /// * `cursor` - The cursor to use as the start for paging
    /// * `limit` - The number of items to attempt to retrieve (not a hard limit for admins)
    /// * `shared` - Shared objects in Thorium
    pub async fn list(
        user: &User,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<GroupList, ApiError> {
        // if user is an admin well need to list all groups
        if user.is_admin() {
            // list all groups in the backend
            db::groups::list(cursor, limit, shared).await
        } else {
            // just use the list of groups the user is apart of
            let names = user
                .groups
                .iter()
                .skip(cursor)
                .take(limit)
                .map(std::borrow::ToOwned::to_owned)
                .collect();

            // calculate new cursor
            let new_cursor = cursor + limit;
            // check if this was the last page
            if new_cursor > user.groups.len() {
                // set cursor to None since no more groups exist
                Ok(GroupList::new(None, names))
            } else {
                // set next cursor
                Ok(GroupList::new(Some(new_cursor), names))
            }
        }
    }

    /// Lists all groups that you have permission to see with details
    ///
    /// # Arguments
    ///
    /// * `user` - The user listing groups
    /// * `cursor` - The cursor to use as the start for paging
    /// * `limit` - The number of items to attempt to retrieve (not a hard limit for admins)
    /// * `shared` - Shared objects in Thorium
    pub async fn list_details(
        user: &User,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<GroupDetailsList, ApiError> {
        // get vector of groups to get details on and the new cursor
        let list = Self::list(user, cursor, limit, shared).await?;
        // get details on the users groups
        let details = db::groups::list_details(list.names.iter(), shared).await?;
        // cast to group details list object
        let group_details = GroupDetailsList::new(list.cursor, details);
        Ok(group_details)
    }

    /// Checks if a array of groups exists in bulk
    ///
    /// If any of these groups do not exist an error will be returned
    ///
    /// # Arguments
    ///
    /// * `names` - An array of group names to check if they exist
    /// * `shared` - Shared objects in Thorium
    pub async fn exists(names: &[String], shared: &Shared) -> Result<(), ApiError> {
        // check if group exists
        let exists = db::groups::exists(names, shared).await?;
        if !exists {
            // build error message
            return not_found!(format!("groups {:?} must exist", &names));
        }
        Ok(())
    }

    /// Checks if a user is an owner in this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to check edit privileges for
    #[instrument(name = "Group::is_owner", skip_all, fields(user = &user.username), err(Debug))]
    pub fn is_owner(&self, user: &User) -> Result<(), ApiError> {
        // if user is an admin then pass check
        if user.is_admin() {
            return Ok(());
        }
        // check if user is an owner
        let has_role = self
            .owners
            .combined
            .iter()
            .any(|name| name == &user.username);
        // return unauthorized if user does not have any of the needed roles
        if !has_role {
            // this user doesn't have any roles in this group
            event!(Level::ERROR, msg = "not an owner",);
            return unauthorized!();
        }
        Ok(())
    }

    /// Checks if a user can delete arbitrary things in a group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to check edit privileges for
    #[instrument(name = "Group::modifiable", skip_all, err(Debug))]
    pub fn modifiable(&self, user: &User) -> Result<(), ApiError> {
        // if user is an admin or a group owner/manager then pass check
        if user.is_admin()
            || self.owners.combined.contains(&user.username)
            || self.managers.combined.contains(&user.username)
        {
            return Ok(());
        }
        // otherwise, this user doesn't have any roles in this group, so return unauthorized
        event!(
            Level::ERROR,
            msg = "cannot modify arbitrary data in group",
            user = user.username
        );
        unauthorized!()
    }

    /// Checks if a user can create and edit images and pipelines in this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to check edit privileges for
    /// * `scaler` - The scaler to check this user can edit/create for
    #[instrument(name = "Group::developer", skip_all, err(Debug))]
    pub fn developer(&self, user: &User, scaler: ImageScaler) -> Result<(), ApiError> {
        // if user is an admin then pass check
        if user.is_admin() {
            return Ok(());
        }
        // make sure this user has the devloper role
        if !user.is_developer(scaler) {
            // this user is not a developer
            event!(
                Level::ERROR,
                msg = "User is not a developer",
                user = user.username,
                scaler = scaler.to_string(),
            );
            return unauthorized!();
        }
        // check if user has any of the roles needed to edit data
        if !self.users.combined.contains(&user.username)
            && !self.managers.combined.contains(&user.username)
            && !self.owners.combined.contains(&user.username)
        {
            // this user doesn't have any roles in this group
            event!(
                Level::ERROR,
                msg = "cannot edit data in group",
                user = user.username
            );
            return unauthorized!();
        }
        Ok(())
    }

    /// Checks if a user can create and edit images and pipelines in this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to check edit privileges for
    /// * `scaler` - The scaler to check this user can edit/create for
    #[instrument(name = "Group::developer_many", skip(self, user), fields(group = self.name), err(Debug))]
    pub fn developer_many(&self, user: &User, scalers: &[ImageScaler]) -> Result<(), ApiError> {
        // if user is an admin then pass check
        if user.is_admin() {
            return Ok(());
        }
        // make sure this user has the devloper role
        if !user.is_developer_many(scalers) {
            // this user is not a developer
            event!(
                Level::ERROR,
                msg = "User is not a developer",
                user = user.username,
            );
            return unauthorized!();
        }
        // check if user has any of the roles needed to edit data
        if !self.users.combined.contains(&user.username)
            && !self.managers.combined.contains(&user.username)
            && !self.owners.combined.contains(&user.username)
        {
            // this user doesn't have any roles in this group
            event!(
                Level::ERROR,
                msg = "cannot edit data in group",
                user = user.username
            );
            return unauthorized!();
        }
        Ok(())
    }

    /// Check if a group allows a certain datatype or not
    #[instrument(name = "Group::allowable", skip(self), fields(group = self.name), err(Debug))]
    pub fn allowable(&self, action: GroupAllowAction) -> Result<(), ApiError> {
        if self.allowed.is_allowed(action) {
            Ok(())
        } else {
            unauthorized!(format!(
                "{} does not allow for {} to be created!",
                self.name, action
            ))
        }
    }

    /// Checks if a user can edit things in this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to check edit privileges for
    #[instrument(name = "Group::editable", skip_all, fields(group = self.name), err(Debug))]
    pub fn editable(&self, user: &User) -> Result<(), ApiError> {
        // if user is an admin then pass check
        if user.is_admin() {
            return Ok(());
        }
        // check if user has any of the roles needed to edit data
        if !self.users.combined.contains(&user.username)
            && !self.analysts.contains(&user.username)
            && !self.managers.combined.contains(&user.username)
            && !self.owners.combined.contains(&user.username)
        {
            // this user doesn't have any roles in this group
            event!(
                Level::ERROR,
                msg = "cannot edit data in group",
                user = user.username
            );
            return unauthorized!();
        }
        Ok(())
    }

    /// Check if a user can see items in this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user try to access data in this group
    #[instrument(name = "Group::viewable", skip_all, err(Debug))]
    pub fn viewable(&self, user: &User) -> Result<(), ApiError> {
        // if user is an admin then pass check
        if user.is_admin() {
            return Ok(());
        }
        // check if user has any of the roles needed to view data
        if !self.users.combined.contains(&user.username)
            && !self.managers.combined.contains(&user.username)
            && !self.analysts.contains(&user.username)
            && !self.owners.combined.contains(&user.username)
            && !self.monitors.combined.contains(&user.username)
        {
            // this user doesn't have any roles in this group
            event!(
                Level::ERROR,
                msg = "no roles in group",
                user = user.username,
                group = &self.name,
            );
            return unauthorized!();
        }
        Ok(())
    }

    /// Authorize a user as part of a group with the required permissions
    ///
    /// Arguments
    ///
    /// * `user` - The user to authorize
    /// * `name` - The name of the group to retrieve
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Group::authorize", skip(user, shared), err(Debug))]
    pub async fn authorize(user: &User, name: &str, shared: &Shared) -> Result<Group, ApiError> {
        // bypass member check if we are an admin
        // make sure user is apart of this group
        if !user.is_admin() && !user.groups.iter().any(|val| val == name) {
            event!(Level::WARN, "User not in group");
            // user is not a part of the requested group
            return unauthorized!();
        }
        // get group object from backend
        // error if it doesn't exist
        let group = match db::groups::get(name, shared).await {
            Ok(group) => group,
            Err(error) => {
                // log that this group doesn't exist
                event!(Level::ERROR, error = error.msg());
                // return our error
                return Err(error);
            }
        };
        // make sure the user can see data in this group
        group.viewable(user)?;
        Ok(group)
    }

    /// Authorize a user as part of all of these group with viewing permissions
    ///
    /// Arguments
    ///
    /// * `user` - The user to authorize
    /// * `name` - The names of the groups to authorize for
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Group::authorize_all", skip(user, shared), err(Debug))]
    pub async fn authorize_all(
        user: &User,
        names: &[String],
        shared: &Shared,
    ) -> Result<Vec<Group>, ApiError> {
        // bypass member check if we are an admin
        // make sure user is apart of this group
        if !user.is_admin() && !names.iter().all(|name| user.groups.contains(name)) {
            // user is not a part of the requested group
            return unauthorized!();
        }
        // get group object from backend
        // error if it doesn't exist
        let groups = db::groups::list_details(names.iter(), shared).await?;
        // make sure the user has some role in this group
        if user.is_admin() {
            // if we are an admin we need to do a second call to make sure these groups exist
            // this is because non existent groups will still return empty user lists
            if !db::groups::exists(names, shared).await? {
                // one or more of the groups don't exist throw an error
                return not_found!(format!("all of {:?} groups must exist", names));
            }
        } else {
            for group in &groups {
                group.viewable(user)?;
            }
            // log that this user is authorized to view this group
            let msg = format!("{} authorized for viewing {:?}", user.username, names);
            event!(Level::INFO, msg);
        }
        Ok(groups)
    }

    /// Authorize a user as part of all of these group with the required permissions based on
    /// the given role check function and all groups allow the given action
    ///
    /// Arguments
    ///
    /// * `user` - The user to authorize
    /// * `name` - The names of the groups to authorize for
    /// * `role_check` - The role check to run on each group
    /// * `role_check_name` - The name of the role check to display in the logs (e.g. 'view', 'edit', etc.)
    /// * `action` - The action the group must allow if one is given
    /// * `shared` - Shared objects in Thorium
    #[instrument(
        name = "Group::authorize_check_allow_all",
        skip(user, role_check, shared),
        err(Debug)
    )]
    pub async fn authorize_check_allow_all<F>(
        user: &User,
        names: &[String],
        role_check: F,
        role_check_name: &str,
        action: Option<GroupAllowAction>,
        shared: &Shared,
    ) -> Result<Vec<Group>, ApiError>
    where
        F: Fn(&Group, &User) -> Result<(), ApiError>,
    {
        // bypass member check if we are an admin
        // make sure user is apart of this group
        if !user.is_admin() && !names.iter().all(|name| user.groups.contains(name)) {
            // user is not a part of the requested group
            return unauthorized!();
        }
        // get group object from backend
        // error if it doesn't exist
        let groups = db::groups::list_details(names.iter(), shared).await?;
        if user.is_admin() {
            if let Some(action) = action {
                // make sure all groups can perform this action even for admins
                for group in &groups {
                    group.allowable(action)?;
                }
            }
            // if we are an admin we need to do a second call to make sure these groups exist
            // this is because non existent groups will still return empty user lists
            if !db::groups::exists(names, shared).await? {
                // one or more of the groups don't exist throw an error
                return not_found!(format!("all of {:?} groups must exist", names));
            }
        } else {
            if let Some(action) = action {
                for group in &groups {
                    // make sure this user can see this group
                    role_check(group, user)?;
                    // make sure this group can perform this action
                    group.allowable(action)?;
                }
            } else {
                for group in &groups {
                    // make sure this user can see this group
                    role_check(group, user)?;
                }
            }
            // log that this user is authorized for this group
            let msg = format!(
                "{} authorized to {} {:?}",
                user.username, role_check_name, names
            );
            event!(Level::INFO, msg);
        }
        Ok(groups)
    }

    /// Check that a user passes a role check in every group of a shared entity and that every
    /// group allows an action
    ///
    /// Unlike [`Group::authorize_check_allow_all`], this checks every group before failing so the
    /// `401` can name the groups that block the user. Only groups the user can see are named;
    /// groups the user can't see are never named or counted.
    ///
    /// # Arguments
    ///
    /// * `user` - The user to check
    /// * `groups` - Every group the entity is in
    /// * `role_check` - The role check to run on each group
    /// * `action` - The action every group must allow if one is given
    /// * `action_name` - The action named in the error (e.g. 'edit' or 'delete')
    /// * `entity` - The name of the entity named in the error
    pub fn check_graph<F>(
        user: &User,
        groups: &[Group],
        role_check: F,
        action: Option<GroupAllowAction>,
        action_name: &str,
        entity: &str,
    ) -> Result<(), ApiError>
    where
        F: Fn(&Group, &User) -> Result<(), ApiError>,
    {
        // track whether any group blocks this user
        let mut blocked = false;
        // track the blocking groups this user can see
        let mut visible = Vec::new();
        // check every group without stopping at the first failure
        for group in groups {
            // a non-admin must hold this group in their (possibly scoped) groups
            let member = user.is_admin() || user.groups.contains(&group.name);
            // run the role check and the allow check for this group
            let passed = member
                && role_check(group, user).is_ok()
                && action.is_none_or(|action| group.allowed.is_allowed(action));
            // record a failing group, naming it only if this user can see it
            if !passed {
                // any failing group blocks this user
                blocked = true;
                // only a group this user can see may be named
                if member && group.viewable(user).is_ok() {
                    visible.push(format!("`{}`", group.name));
                }
            }
        }
        // build the error if any group blocks this user
        if blocked {
            // build the message without any group this user can't see
            let msg = if visible.is_empty() {
                format!("you don't have permission to {action_name} `{entity}`")
            } else {
                format!(
                    "you don't have permission to {action_name} `{entity}` in {}",
                    visible.join(", ")
                )
            };
            // log and return the refusal
            event!(Level::ERROR, msg = &msg, user = user.username);
            return unauthorized!(msg);
        }
        Ok(())
    }

    /// Authorize a user in every group of a shared entity with the required role and allowed
    /// action, returning the groups
    ///
    /// Used for `Contained` entities, whose edits and deletes need rights in all of their groups.
    /// A refusal is a `401` naming only the blocking groups the user can see.
    ///
    /// # Arguments
    ///
    /// * `user` - The user to authorize
    /// * `names` - The names of every group the entity is in
    /// * `role_check` - The role check to run on each group
    /// * `action` - The action every group must allow if one is given
    /// * `action_name` - The action named in the error (e.g. 'edit' or 'delete')
    /// * `entity` - The name of the entity named in the error
    /// * `shared` - Shared objects in Thorium
    #[instrument(
        name = "Group::authorize_graph",
        skip(user, role_check, shared),
        err(Debug)
    )]
    pub async fn authorize_graph<F>(
        user: &User,
        names: &[String],
        role_check: F,
        action: Option<GroupAllowAction>,
        action_name: &str,
        entity: &str,
        shared: &Shared,
    ) -> Result<Vec<Group>, ApiError>
    where
        F: Fn(&Group, &User) -> Result<(), ApiError>,
    {
        // get the details for every group
        let groups = db::groups::list_details(names.iter(), shared).await?;
        // admins pass every role check, so make sure these groups actually exist
        if user.is_admin() && !db::groups::exists(names, shared).await? {
            return not_found!(format!("all of {names:?} groups must exist"));
        }
        // check every group and name only the visible ones that block this user
        Self::check_graph(user, &groups, role_check, action, action_name, entity)?;
        Ok(groups)
    }

    /// Get a group object if that group exists
    ///
    /// # Arguments
    ///
    /// * `user` - The user to authorize to be able to view this group
    /// * `name` - The name of the group to get
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Group::get", skip(user, shared), err(Debug))]
    pub async fn get(user: &User, name: &str, shared: &Shared) -> Result<Self, ApiError> {
        // authorize user is apart of this group
        Self::authorize(user, name, shared).await
    }

    /// Diffs two group versions and finds which users to add or remove from group perms
    ///
    /// # Arguments
    ///
    /// * `old` - The old group to compare too
    #[instrument(name = "Group::diff", skip_all)]
    fn diff(&self, old: &Group, added: &mut HashSet<String>, removed: &mut HashSet<String>) {
        // get the difference between our new and old group
        let owners_add = self.owners.combined.difference(&old.owners.combined);
        let owners_remove = old.owners.combined.difference(&self.owners.combined);
        let managers_add = self.managers.combined.difference(&old.managers.combined);
        let managers_remove = old.managers.combined.difference(&self.managers.combined);
        let users_add = self.users.combined.difference(&old.users.combined);
        let users_remove = old.users.combined.difference(&self.users.combined);
        let monitors_add = self.monitors.combined.difference(&old.monitors.combined);
        let monitors_remove = old.monitors.combined.difference(&self.monitors.combined);
        // ignore any users that are direct add users
        let owners_remove = owners_remove.filter(|name| !old.owners.direct.contains(*name));
        let managers_remove = managers_remove.filter(|name| !old.managers.direct.contains(*name));
        let users_remove = users_remove.filter(|name| !old.users.direct.contains(*name));
        let monitors_remove = monitors_remove.filter(|name| !old.monitors.direct.contains(*name));
        // extend our add set
        added.extend(owners_add.cloned());
        added.extend(managers_add.cloned());
        added.extend(users_add.cloned());
        added.extend(monitors_add.cloned());
        // extend our remove set
        removed.extend(owners_remove.cloned());
        removed.extend(managers_remove.cloned());
        removed.extend(users_remove.cloned());
        removed.extend(monitors_remove.cloned());
    }

    /// Updates the group
    ///
    /// # Arguments
    ///
    /// * `update` - The updates to apply to this group
    /// * `user` - The user that is updating this group
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Group::update", skip_all, err(Debug))]
    pub async fn update(
        mut self,
        mut update: GroupUpdate,
        user: &User,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // error if ldap is not configured but new ldap roles are
        if shared.config.thorium.auth.ldap.is_none() && update.will_update_metagroups() {
            return unavailable!("Ldap is not configured".to_owned());
        }
        // list of users added/removed
        let mut added = HashSet::default();
        let mut removed = HashSet::default();
        // get a copy of our old group info
        let old = self.clone();
        // get a list of all valid users in Thorium to validate against
        let valid = HashSet::from_iter(db::users::list(shared).await?);
        // make sure we can modify this group with our role
        // if we are adding/removing owners we need to also be an owner
        if update.owners.is_empty() {
            // changing group-wide settings (the description or the `allowed` flags) is an
            // owner/manager action, so treat a settings-only update as privileged too. Otherwise the
            // `modifiable` check below is skipped whenever the update touches no role membership, and
            // any member — even a read-only monitor — could flip group settings or deface the
            // description.
            let changes_settings = update.description.is_some()
                || update.clear_description
                || !update.allowed.is_empty();
            // only check manager if we are doing more then just removing ourselves, or the update
            // changes group settings
            if changes_settings || !update.removes_only_user(&user.username) {
                // we are doing more than removing ourselves (or changing settings) so require
                // owner/manager perms
                self.modifiable(user)?;
            }
        } else {
            // make sure we are an owner
            self.is_owner(user)?;
            // error if we try to remove ourselves as owner
            if update.owners.direct_remove.contains(&user.username) {
                return conflict!("You cannot remove yourself as an owner".to_owned());
            }
            // update our owner roles
            update
                .owners
                .update_direct(&mut self.owners, &mut added, &mut removed, &valid)?;
            // error if the owner list will be empty after this
            if self.owners.direct.is_empty() && self.owners.metagroups.is_empty() {
                return conflict!("You cannot update a group to have no owners".to_owned());
            }
        }
        // start with empty direct + metagroup user lists
        self.owners.combined.clear();
        self.managers.combined.clear();
        self.users.combined.clear();
        self.monitors.combined.clear();
        // apply the updates for this groups metagroups
        update.owners.update_metagroups(&mut self.owners)?;
        update.managers.update_metagroups(&mut self.managers)?;
        update.users.update_metagroups(&mut self.users)?;
        update.monitors.update_metagroups(&mut self.monitors)?;
        // get the ldap info for this group if we have any ldap info
        if self.metagroup_enabled() {
            // get the ldap info for this groups metagroups
            let ldap_info = LdapUserMap::new(&[&self], &valid, shared).await?;
            // apply this ldap info to our roles
            ldap_info.apply_to_role(&mut self.owners);
            ldap_info.apply_to_role(&mut self.managers);
            ldap_info.apply_to_role(&mut self.users);
            ldap_info.apply_to_role(&mut self.monitors);
        }
        // figure out which users to add or remove
        self.diff(&old, &mut added, &mut removed);
        // update this groups roles
        update
            .owners
            .update_direct(&mut self.owners, &mut added, &mut removed, &valid)?;
        update
            .managers
            .update_direct(&mut self.managers, &mut added, &mut removed, &valid)?;
        update
            .users
            .update_direct(&mut self.users, &mut added, &mut removed, &valid)?;
        update
            .monitors
            .update_direct(&mut self.monitors, &mut added, &mut removed, &valid)?;
        // fix any overlap in this updates roles
        self.fix();
        // update description
        update_opt_empty!(self.description, update.description);
        // clear description if flag is set
        update_clear!(self.description, update.clear_description);
        // update our allowed settings
        update.allowed.update(&mut self);
        // save updated group to the backend
        db::groups::update(&self, &added, &removed, shared).await?;
        Ok(self)
    }

    /// Deletes a group by name
    ///
    /// This will also delete all pipelines, images, and reactions within this group.
    ///
    /// # Arguments
    ///
    /// * `user` - The user to authorize to be able to delete this group
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Group::delete", skip_all, fields(group = &self.name), err(Debug))]
    pub async fn delete(self, user: &User, shared: &Shared) -> Result<(), ApiError> {
        // make sure we are an owner of this group
        self.is_owner(user)?;
        // delete from backend
        db::groups::delete(user, &self, shared).await
    }

    /// Syncs all ldap metagroups and their users
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Group::sync_ldap", skip_all, err(Debug))]
    pub async fn sync_ldap(shared: &Shared) -> Result<(), ApiError> {
        // we are syncing all groups in Thorium so get the Thorium user since
        // they can see all groups.
        let admin = User::force_get("thorium", shared).await?;
        // list all of our groups and update them one at a time
        for group in db::groups::list_all(&admin, shared).await? {
            // make an empty group update to apply to this group
            let update = GroupUpdate::default();
            // apply this empty update to force an ldap update
            group.update(update, &admin, shared).await?;
        }
        Ok(())
    }

    /// Get stats on a group's shared workload jobs by tool instance and user
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Group::stats", skip(shared), fields(group = &self.name), err(Debug))]
    pub async fn stats(&self, shared: &Shared) -> Result<GroupStats, ApiError> {
        // get a list of the members of this group
        let members = self.members();
        // count each tool instance's shared jobs for each member
        let workloads = db::jobs::workload_stats(&self.name, &members, shared).await?;
        Ok(GroupStats { workloads })
    }
}

impl TryFrom<RawGroupData> for Group {
    type Error = ApiError;

    /// Cast raw data to a Group
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw data returned from the db including name, `MembersLists`,
    ///   and the data map and analysts
    fn try_from(raw: RawGroupData) -> Result<Self, Self::Error> {
        let (name, members, data, analysts) = raw;
        // build our owners object
        let owners = GroupUsers {
            combined: HashSet::from_iter(members.0),
            direct: HashSet::from_iter(members.1),
            metagroups: HashSet::from_iter(members.2),
        };
        // build our managers object
        let managers = GroupUsers {
            combined: HashSet::from_iter(members.3),
            direct: HashSet::from_iter(members.4),
            metagroups: HashSet::from_iter(members.5),
        };
        // build our users object
        let users = GroupUsers {
            combined: HashSet::from_iter(members.6),
            direct: HashSet::from_iter(members.7),
            metagroups: HashSet::from_iter(members.8),
        };
        // build our monitors object
        let monitors = GroupUsers {
            combined: HashSet::from_iter(members.9),
            direct: HashSet::from_iter(members.10),
            metagroups: HashSet::from_iter(members.11),
        };
        // cast to a group object
        let group = Group {
            name,
            owners,
            managers,
            analysts,
            users,
            monitors,
            description: deserialize_opt!(data, "description"),
            allowed: deserialize_ext!(data, "allowed", GroupAllowed::default()),
        };
        Ok(group)
    }
}

// The lifetime required for &HashSet appear to confuse the compiler so just
// write this tuple out manually instead of using a type
impl
    TryFrom<(
        String,
        MembersLists,
        HashMap<String, String>,
        &HashSet<String>,
    )> for Group
{
    type Error = ApiError;

    /// Cast raw data to a Group where analysts is a &hashset
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw data returned from the db including name, `MembersLists`,
    ///   and the data map, and analysts
    fn try_from(
        raw: (
            String,
            MembersLists,
            HashMap<String, String>,
            &HashSet<String>,
        ),
    ) -> Result<Self, Self::Error> {
        let (name, members, data, analysts) = raw;
        // build our owners object
        let owners = GroupUsers {
            combined: HashSet::from_iter(members.0),
            direct: HashSet::from_iter(members.1),
            metagroups: HashSet::from_iter(members.2),
        };
        // build our managers object
        let managers = GroupUsers {
            combined: HashSet::from_iter(members.3),
            direct: HashSet::from_iter(members.4),
            metagroups: HashSet::from_iter(members.5),
        };
        // build our users object
        let users = GroupUsers {
            combined: HashSet::from_iter(members.6),
            direct: HashSet::from_iter(members.7),
            metagroups: HashSet::from_iter(members.8),
        };
        // build our monitors object
        let monitors = GroupUsers {
            combined: HashSet::from_iter(members.9),
            direct: HashSet::from_iter(members.10),
            metagroups: HashSet::from_iter(members.11),
        };
        // cast to a group object
        let group = Group {
            name,
            owners,
            managers,
            analysts: analysts.clone(),
            users,
            monitors,
            description: deserialize_opt!(data, "description"),
            allowed: deserialize_ext!(data, "allowed", GroupAllowed::default()),
        };
        Ok(group)
    }
}

// Build the ldap filter for a specific key/value
//
// # Arguments
//
// * `arr` - The source array of objects
// * `key` - The name of the value to filter on
// * `target` - The target hashset to insert into
macro_rules! build_filter {
    ($arr:expr, $key:expr, $target:expr) => {
        for item in $arr.iter() {
            $target.insert(format!("({}={})", $key, ldap3::ldap_escape(item)));
        }
    };
}

/// A map of users in ldap metagroups
#[derive(Debug)]
pub(super) struct LdapUserMap {
    users: HashMap<String, Vec<String>>,
}

impl LdapUserMap {
    /// Crawls all groups in Thorium for ldap metagroups and builds a metagroup user map
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to sync against LDAP
    /// * `valid_users` - A set of existing users in Thorium
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "LdapUserMap::new", skip_all, err(Debug))]
    pub async fn new(
        groups: &[&Group],
        valid_users: &HashSet<String>,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        if let Some(conf) = &shared.config.thorium.auth.ldap {
            // crawl over all groups and build a list of filters
            let mut group_filters = HashSet::new();
            // build a set of all ldap based groups to sync
            for group in groups {
                build_filter!(&group.owners.metagroups, "cn", group_filters);
                build_filter!(&group.managers.metagroups, "cn", group_filters);
                build_filter!(&group.users.metagroups, "cn", group_filters);
                build_filter!(&group.monitors.metagroups, "cn", group_filters);
            }
            // if we built no filters then return an empty user map
            if group_filters.is_empty() {
                return Ok(LdapUserMap {
                    users: HashMap::new(),
                });
            }
            // build the specific group filters
            let group_filters = itertools::join(&group_filters, "");
            let filter = format!("(&(|{group_filters})(objectClass=*))");
            //  build an ldap connection
            //  we do this on demand instead of having it in shared because it needs to be mutable
            let (conn, mut ldap) = ldap!(conf).await?;
            ldap3::drive!(conn);
            // if we have an ldap service account then bind with that
            super::users::bind_to_ldap_system_user(&mut ldap, conf).await?;
            // start searching ldap
            let mut stream = ldap
                .streaming_search(&conf.scope, Scope::Subtree, &filter, vec!["*"])
                .await?;
            // crawl over ldap entries and get the users in these ldap metagroups
            let mut user_map = HashMap::new();
            while let Some(entry) = stream.next().await? {
                // cast this to a search entry
                let mut entry = SearchEntry::construct(entry);
                // get this groups name  and users if we can extract it
                if let (Some(name), Some(users)) = (
                    Self::group_name_extract(&entry.dn),
                    entry.attrs.remove(&conf.group_members_attr),
                ) {
                    // extract our usernames if necesary
                    let mut users = Self::group_member_extract(users, conf);
                    // trim the user list down to only valid Thorium users
                    users.retain(|name| valid_users.contains(name));
                    user_map.insert(name.to_owned(), users);
                }
            }
            // finish and abandon the search to prevent wasted resources on the ldap server
            stream.finish().await.success()?;
            let msgid = stream.ldap_handle().last_id();
            ldap.abandon(msgid).await?;
            ldap.unbind().await?;
            Ok(LdapUserMap { users: user_map })
        } else {
            unavailable!("Ldap is not configured!".to_owned())
        }
    }

    /// extract a group name from an ldap search entry
    ///
    /// # Arguments
    ///
    /// * `raw` - The utf-8 encoded ldap search entry
    fn group_name_extract(raw: &str) -> Option<&str> {
        // make this starts with a cn=
        if raw.starts_with("cn=") {
            // set the start to just past the cn=
            let mut start = 3;
            loop {
                // get the index the first ',' or stop if there isn't one
                let end = raw[start..].find(',')?;
                // check to see if this comma was escaped
                if &raw[end - 1..end - 1] == "\\" {
                    // this comma was escaped skip it and keep looking
                    start = end;
                } else {
                    // we found the end index of our group name return it
                    // add start to our end value since end is a relative index
                    return Some(&raw[3..start + end]);
                }
            }
        }
        // this ldap entry does not start with cn= so ignore it
        None
    }

    /// Extract a username from a group membership row
    #[instrument(name = "LdapUserMap::group_member_extract", skip_all)]
    fn group_member_extract(raw: Vec<String>, conf: &crate::conf::Ldap) -> Vec<String> {
        // get the field to extract our group members usernames from
        if let Some(key_name) = &conf.group_member_field {
            // build a list to store our extracted usernames
            let mut extracted = Vec::with_capacity(raw.len());
            // add the = to our field
            let key = format!("{key_name}=");
            // crawl over our group member rows
            for row in &raw {
                // try to extract the username field from this row
                if let Some(field) = row.split(',').find(|field| field.starts_with(&key)) {
                    // remove the field key form our username field
                    let username = field[key.len()..].to_owned();
                    // add our extracted username
                    extracted.push(username);
                } else {
                    // log that we failed to extract a username
                    event!(
                        Level::ERROR,
                        error = true,
                        error_msg = "Failed to extract username",
                        row
                    );
                }
            }
            extracted
        } else {
            raw
        }
    }

    /// Apply this update to a target roles groups
    pub fn apply_to_role(&self, role: &mut GroupUsers) {
        // crawl the metagroups that have this role and add them
        for metagroup in &role.metagroups {
            // get the users in this metagroup
            if let Some(users) = self.users.get(metagroup) {
                // extend our combined users with the users in this metagroup
                role.combined
                    .extend(users.iter().map(std::borrow::ToOwned::to_owned));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::http::StatusCode;
    use chrono::Utc;
    use std::collections::{HashMap, HashSet};

    use crate::models::groups::GroupUsers;
    use crate::models::{
        Group, GroupAllowAction, GroupAllowed, GroupAllowedUpdate, User, UserRole, UserSettings,
    };

    /// Build a role's members from usernames
    ///
    /// # Arguments
    ///
    /// * `names` - The usernames that hold this role
    fn members(names: &[&str]) -> GroupUsers {
        // build the set of members
        let combined: HashSet<String> = names.iter().map(|name| (*name).to_string()).collect();
        // add them all as direct members
        GroupUsers {
            combined: combined.clone(),
            direct: combined,
            metagroups: HashSet::default(),
        }
    }

    /// Build a group for tests
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the group
    /// * `owners` - The owners of the group
    /// * `users` - The users of the group
    /// * `monitors` - The monitors of the group
    pub(crate) fn group(name: &str, owners: &[&str], users: &[&str], monitors: &[&str]) -> Group {
        // build a group with no managers or analysts that allows everything
        Group {
            name: name.to_owned(),
            owners: members(owners),
            managers: members(&[]),
            analysts: HashSet::default(),
            users: members(users),
            monitors: members(monitors),
            description: None,
            allowed: GroupAllowed::default(),
        }
    }

    /// Build a user for tests
    ///
    /// # Arguments
    ///
    /// * `username` - The name of the user
    /// * `role` - The user's role
    /// * `groups` - The groups the user is in
    pub(crate) fn user(username: &str, role: UserRole, groups: &[&str]) -> User {
        // build a verified user with default settings
        User {
            username: username.to_owned(),
            password: None,
            email: format!("{username}@example.com"),
            role,
            groups: groups.iter().map(|group| (*group).to_string()).collect(),
            actual_groups: Vec::default(),
            token: String::default(),
            token_expiration: Utc::now(),
            unix: None,
            settings: UserSettings::default(),
            verified: true,
            verification_token: None,
            verification_sent: None,
            aliases: HashMap::default(),
            image: None,
        }
    }

    /// Get the message of a failed graph check
    ///
    /// # Arguments
    ///
    /// * `user` - The user to check
    /// * `groups` - The groups to check
    fn denied(user: &User, groups: &[Group]) -> String {
        // run the check and expect a 401
        let err = Group::check_graph(
            user,
            groups,
            Group::editable,
            Some(GroupAllowAction::Entities),
            "edit",
            "tool",
        )
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        err.msg().unwrap_or_default()
    }

    /// A user who can edit every group passes the graph check
    #[test]
    fn graph_check_passes_in_every_group() {
        // a user in both groups
        let bob = user("bob", UserRole::User, &["a", "b"]);
        // both groups grant edit rights
        let groups = vec![
            group("a", &[], &["bob"], &[]),
            group("b", &["bob"], &[], &[]),
        ];
        // the check passes
        assert!(
            Group::check_graph(
                &bob,
                &groups,
                Group::editable,
                Some(GroupAllowAction::Entities),
                "edit",
                "tool"
            )
            .is_ok()
        );
    }

    /// A visible group that blocks the edit is named in the message
    #[test]
    fn graph_check_names_visible_blockers() {
        // a user who only monitors group b
        let bob = user("bob", UserRole::User, &["a", "b"]);
        // group a grants edit rights and group b doesn't
        let groups = vec![
            group("a", &[], &["bob"], &[]),
            group("b", &[], &[], &["bob"]),
        ];
        // the message names b only
        let msg = denied(&bob, &groups);
        assert_eq!(msg, "you don't have permission to edit `tool` in `b`");
    }

    /// A group the caller can't see is never named or counted
    #[test]
    fn graph_check_hides_invisible_blockers() {
        // a user who isn't in group secret
        let bob = user("bob", UserRole::User, &["a"]);
        // group a grants edit rights and secret holds other users only
        let groups = vec![
            group("a", &[], &["bob"], &[]),
            group("secret", &["alice"], &[], &[]),
        ];
        // the message doesn't name or count the hidden group
        let msg = denied(&bob, &groups);
        assert_eq!(msg, "you don't have permission to edit `tool`");
    }

    /// A group listed in the membership lists but outside a scoped user's groups is hidden too
    #[test]
    fn graph_check_respects_scoped_groups() {
        // a user whose token only covers group a
        let bob = user("bob", UserRole::User, &["a"]);
        // bob holds roles in both groups
        let groups = vec![
            group("a", &[], &["bob"], &[]),
            group("b", &[], &["bob"], &[]),
        ];
        // b blocks the edit but is outside bob's token so it isn't named
        let msg = denied(&bob, &groups);
        assert_eq!(msg, "you don't have permission to edit `tool`");
    }

    /// A visible group that disallows the action is named, even for admins
    #[test]
    fn graph_check_names_disallowed_groups() {
        // a group that disallows entities
        let mut closed = group("b", &["bob"], &[], &[]);
        closed.allowed.entities = false;
        // the other group allows them
        let groups = vec![group("a", &["bob"], &[], &[]), closed];
        // a member is told which group blocks it
        let bob = user("bob", UserRole::User, &["a", "b"]);
        assert_eq!(
            denied(&bob, &groups),
            "you don't have permission to edit `tool` in `b`"
        );
        // an admin is blocked by the allow flag too
        let admin = user("admin", UserRole::Admin, &[]);
        assert_eq!(
            denied(&admin, &groups),
            "you don't have permission to edit `tool` in `b`"
        );
        // without an action to check the admin passes
        assert!(Group::check_graph(&admin, &groups, Group::editable, None, "edit", "tool").is_ok());
    }

    /// Applying an associations update flips only the associations flag
    #[test]
    fn allowed_update_applies_associations() {
        // a group that allows everything
        let mut target = group("a", &["bob"], &[], &[]);
        // disallow associations in this group
        GroupAllowedUpdate::default()
            .disable_associations()
            .update(&mut target);
        // only the associations flag changed
        assert!(!target.allowed.associations);
        assert!(target.allowed.entities && target.allowed.files && target.allowed.tags);
        // allow associations again
        GroupAllowedUpdate::default()
            .enable_associations()
            .update(&mut target);
        assert!(target.allowed.associations);
    }

    /// `group_name_extract` returns the `cn` value up to the first comma and ignores entries
    /// that don't start with `cn=` or have no comma
    #[test]
    fn group_name_extract_reads_the_cn() {
        // a group entry returns its cn value
        assert_eq!(
            super::LdapUserMap::group_name_extract("cn=analysts,ou=groups,dc=example"),
            Some("analysts")
        );
        // an entry with no comma after the cn has no group name
        assert_eq!(super::LdapUserMap::group_name_extract("cn=analysts"), None);
        // an entry that isn't a cn is ignored
        assert_eq!(
            super::LdapUserMap::group_name_extract("uid=bob,ou=people"),
            None
        );
    }
}
