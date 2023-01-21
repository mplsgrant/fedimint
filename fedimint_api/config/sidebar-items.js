window.SIDEBAR_ITEMS = {"enum":[["DkgMessage",""],["DkgPeerMsg","Things that a `distributed_gen` config can send between peers"],["DkgStep",""],["SupportedDkgMessage","`enum` version of [`SupportedDkgMessage`]"]],"fn":[["scalar","PeerIds are offset by 1, since evaluating a poly at 0 reveals the secret"]],"mod":[["serde_commit","Handling the Group serialization with a wrapper"]],"struct":[["ApiEndpoint",""],["ClientConfig","Total client config"],["ClientModuleConfig","Config for the client-side of a particular Federation module"],["ConfigGenParams","Global Fedimint configuration generation settings passed to modules"],["ConfigResponse","The API response for configuration requests"],["Dkg",""],["DkgKeys",""],["DkgRunner",""],["FederationId","The federation id is a copy of the authentication threshold public key of the federation"],["JsonWithKind","[`serde_json::Value`] that must contain `kind: String` field"],["LegacyInitOrderIter","Iterate over module generators in a legacy, hardcoded order: ln, mint, wallet, rest… Returning each `kind` exactly once, so that `LEGACY_HARDCODED_` constants correspond to correct module kind."],["ModuleConfigResponse","Response from the API for this particular module"],["ModuleGenRegistry",""],["ServerModuleConfig","Config for the server-side of a particular Federation module"],["ThresholdKeys","Our secret key share of a threshold key"]],"trait":[["DkgGroup","Defines a group (e.g. G1 or G2) that we can generate keys for"],["ISupportedDkgMessage","Supported (by Fedimint’s code) `DkgMessage<T>` types"],["ModuleGenParams",""],["SGroup",""],["TypedClientModuleConfig","Typed client side module config"],["TypedServerModuleConfig","Module (server side) config"],["TypedServerModuleConsensusConfig","Consensus-critical part of a server side module config"]]};