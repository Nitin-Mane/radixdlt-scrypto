mod auth_zone;
mod authorization;
mod bucket;
mod mint_params;
mod non_fungible;
mod non_fungible_address;
mod non_fungible_data;
mod non_fungible_id;
mod proof;
mod proof_rule;
mod resource_builder;
mod resource_def;
mod resource_type;
mod schema_path;
mod system;
mod vault;

/// Resource flags.
pub mod resource_flags;
/// Resource permissions.
pub mod resource_permissions;

pub use auth_zone::AuthZone;
pub use authorization::ComponentAuthorization;
pub use bucket::{Bucket, ParseBucketError};
pub use mint_params::MintParams;
pub use non_fungible::NonFungible;
pub use non_fungible_address::{NonFungibleAddress, ParseNonFungibleAddressError};
pub use non_fungible_data::NonFungibleData;
pub use non_fungible_id::{NonFungibleId, ParseNonFungibleIdError};
pub use proof::{ParseProofError, Proof};
pub use proof_rule::{
    require, require_all_of, require_amount, require_any_of, require_n_of, AuthRule, ProofRule,
    SoftDecimal, SoftCount, SoftResource, SoftResourceOrNonFungible, SoftResourceOrNonFungibleList,
};
pub use resource_builder::{ResourceBuilder, DIVISIBILITY_MAXIMUM, DIVISIBILITY_NONE};
pub use resource_def::{ParseResourceDefIdError, ResourceDef, ResourceDefId};
pub use resource_flags::*;
pub use resource_permissions::*;
pub use resource_type::ResourceType;
pub use schema_path::SchemaPath;
pub use system::{init_resource_system, resource_system, ResourceSystem};
pub use vault::{ParseVaultError, Vault};
