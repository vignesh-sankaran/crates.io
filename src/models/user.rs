use diesel::dsl::now;
use diesel::prelude::*;
use std::borrow::Cow;

use app::App;
use util::CargoResult;

use models::{Crate, CrateOwner, NewEmail, Owner, OwnerKind, Rights};
use schema::{crate_owners, emails, users};
use views::{EncodablePrivateUser, EncodablePublicUser};

/// The model representing a row in the `users` database table.
#[derive(Clone, Debug, PartialEq, Eq, Queryable, Identifiable, AsChangeset, Associations)]
pub struct User {
    pub id: i32,
    pub email: Option<String>,
    pub gh_access_token: String,
    pub gh_login: String,
    pub name: Option<String>,
    pub gh_avatar: Option<String>,
    pub gh_id: i32,
}

#[derive(Insertable, Debug)]
#[table_name = "users"]
pub struct NewUser<'a> {
    pub gh_id: i32,
    pub gh_login: &'a str,
    pub email: Option<&'a str>,
    pub name: Option<&'a str>,
    pub gh_avatar: Option<&'a str>,
    pub gh_access_token: Cow<'a, str>,
}

impl<'a> NewUser<'a> {
    pub fn new(
        gh_id: i32,
        gh_login: &'a str,
        email: Option<&'a str>,
        name: Option<&'a str>,
        gh_avatar: Option<&'a str>,
        gh_access_token: &'a str,
    ) -> Self {
        NewUser {
            gh_id,
            gh_login,
            email,
            name,
            gh_avatar,
            gh_access_token: Cow::Borrowed(gh_access_token),
        }
    }

    /// Inserts the user into the database, or updates an existing one.
    pub fn create_or_update(&self, conn: &PgConnection) -> QueryResult<User> {
        use diesel::dsl::sql;
        use diesel::insert_into;
        use diesel::pg::upsert::excluded;
        use diesel::sql_types::Integer;
        use diesel::NotFound;
        use schema::users::dsl::*;

        conn.transaction(|| {
            let user = insert_into(users)
                .values(self)
                // We need the `WHERE gh_id > 0` condition here because `gh_id` set
                // to `-1` indicates that we were unable to find a GitHub ID for
                // the associated GitHub login at the time that we backfilled
                // GitHub IDs. Therefore, there are multiple records in production
                // that have a `gh_id` of `-1` so we need to exclude those when
                // considering uniqueness of `gh_id` values. The `> 0` condition isn't
                // necessary for most fields in the database to be used as a conflict
                // target :)
                .on_conflict(sql::<Integer>("(gh_id) WHERE gh_id > 0"))
                .do_update()
                .set((
                    gh_login.eq(excluded(gh_login)),
                    name.eq(excluded(name)),
                    gh_avatar.eq(excluded(gh_avatar)),
                    gh_access_token.eq(excluded(gh_access_token)),
                ))
                .get_result::<User>(conn)?;

            // To send the user an account verification email...
            if let Some(user_email) = user.email.as_ref() {
                let new_email = NewEmail {
                    user_id: user.id,
                    email: user_email,
                };

                let token = insert_into(emails::table)
                    .values(&new_email)
                    .on_conflict_do_nothing()
                    .returning(emails::token)
                    .get_result::<String>(conn)
                    .optional()?;

                if let Some(token) = token {
                    ::email::send_user_confirm_email(user_email, &user.gh_login, &token)
                        .map_err(|_| NotFound)?;
                }
            }

            Ok(user)
        })
    }
}

impl User {
    /// Queries the database for a user with a certain `api_token` value.
    pub fn find_by_api_token(conn: &PgConnection, token_: &str) -> CargoResult<User> {
        use diesel::update;
        use schema::api_tokens::dsl::{api_tokens, last_used_at, revoked, token, user_id};
        use schema::users::dsl::{id, users};
        let tokens = api_tokens
            .filter(token.eq(token_))
            .filter(revoked.eq(false));
        let user_id_ = update(tokens)
            .set(last_used_at.eq(now.nullable()))
            .returning(user_id)
            .get_result::<i32>(conn)?;
        Ok(users.filter(id.eq(user_id_)).get_result(conn)?)
    }

    pub fn owning(krate: &Crate, conn: &PgConnection) -> CargoResult<Vec<Owner>> {
        let base_query = CrateOwner::belonging_to(krate).filter(crate_owners::deleted.eq(false));
        let users = base_query
            .inner_join(users::table)
            .select(users::all_columns)
            .filter(crate_owners::owner_kind.eq(OwnerKind::User as i32))
            .load(conn)?
            .into_iter()
            .map(Owner::User);

        Ok(users.collect())
    }

    /// Given this set of owners, determines the strongest rights the
    /// user has.
    ///
    /// Shortcircuits on `Full` because you can't beat it. In practice we'll always
    /// see `[user, user, user, ..., team, team, team]`, so we could shortcircuit on
    /// `Publish` as well, but this is a non-obvious invariant so we don't bother.
    /// Sweet free optimization if teams are proving burdensome to check.
    /// More than one team isn't really expected, though.
    pub fn rights(&self, app: &App, owners: &[Owner]) -> CargoResult<Rights> {
        let mut best = Rights::None;
        for owner in owners {
            match *owner {
                Owner::User(ref other_user) => {
                    if other_user.id == self.id {
                        return Ok(Rights::Full);
                    }
                }
                Owner::Team(ref team) => {
                    if team.contains_user(app, self)? {
                        best = Rights::Publish;
                    }
                }
            }
        }
        Ok(best)
    }

    pub fn has_verified_email(&self, conn: &PgConnection) -> CargoResult<bool> {
        use diesel::dsl::exists;
        let email_exists = diesel::select(exists(
            emails::table
                .filter(emails::user_id.eq(self.id))
                .filter(emails::verified.eq(true)),
        ))
        .get_result(&*conn)?;
        Ok(email_exists)
    }

    /// Converts this `User` model into an `EncodablePrivateUser` for JSON serialization.
    pub fn encodable_private(
        self,
        email_verified: bool,
        email_verification_sent: bool,
    ) -> EncodablePrivateUser {
        let User {
            id,
            email,
            name,
            gh_login,
            gh_avatar,
            ..
        } = self;
        let url = format!("https://github.com/{}", gh_login);
        EncodablePrivateUser {
            id,
            email,
            email_verified,
            email_verification_sent,
            avatar: gh_avatar,
            login: gh_login,
            name,
            url: Some(url),
        }
    }

    /// Converts this`User` model into an `EncodablePublicUser` for JSON serialization.
    pub fn encodable_public(self) -> EncodablePublicUser {
        let User {
            id,
            name,
            gh_login,
            gh_avatar,
            ..
        } = self;
        let url = format!("https://github.com/{}", gh_login);
        EncodablePublicUser {
            id,
            avatar: gh_avatar,
            login: gh_login,
            name,
            url: Some(url),
        }
    }
}
