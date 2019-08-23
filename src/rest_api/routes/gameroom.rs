// Copyright 2019 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use std::collections::HashMap;

use actix_web::{web, Error, HttpResponse};
use futures::{Future, IntoFuture};
use libsplinter::admin::messages::{
    AuthorizationType, CreateCircuit, DurabilityType, PersistenceType, RouteType, SplinterNode,
    SplinterService,
};
use libsplinter::node_registry::Node;
use libsplinter::protos::admin::{
    CircuitManagementPayload, CircuitManagementPayload_Action as Action,
    CircuitManagementPayload_Header as Header,
};
use openssl::hash::{hash, MessageDigest};
use protobuf::Message;
use uuid::Uuid;

use crate::rest_api::RestApiResponseError;

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateGameroomForm {
    alias: String,
    member: Vec<GameroomMember>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GameroomMember {
    identity: String,
    metadata: MemberMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemberMetadata {
    organization: String,
    endpoint: String,
    public_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApplicationMetadata {
    alias: String,
}

pub fn propose_gameroom(
    create_gameroom: web::Json<CreateGameroomForm>,
    node_info: web::Data<Node>,
) -> impl Future<Item = HttpResponse, Error = Error> {
    let mut members = create_gameroom
        .member
        .iter()
        .map(|node| SplinterNode {
            node_id: node.identity.to_string(),
            endpoint: node.metadata.endpoint.to_string(),
        })
        .collect::<Vec<SplinterNode>>();

    members.push(SplinterNode {
        node_id: node_info.identity.to_string(),
        endpoint: node_info
            .metadata
            .get("endpoint")
            .unwrap_or(&"".to_string())
            .to_string(),
    });

    let partial_circuit_id = members.iter().fold(String::new(), |mut acc, member| {
        acc.push_str(&format!("::{}", member.node_id));
        acc
    });

    let application_metadata = match make_application_metadata(&create_gameroom.alias) {
        Ok(bytes) => bytes,
        Err(err) => {
            debug!("Failed to serialize application metadata: {}", err);
            return HttpResponse::InternalServerError().finish().into_future();
        }
    };

    let scabbard_admin_keys = match serde_json::to_string(
        &create_gameroom
            .member
            .iter()
            .map(|member| member.metadata.public_key.clone())
            .collect::<Vec<_>>(),
    ) {
        Ok(s) => s,
        Err(err) => {
            return HttpResponse::InternalServerError()
                .json(format!("failed to serialize member public keys: {}", err))
                .into_future()
        }
    };
    let mut scabbard_args = HashMap::new();
    scabbard_args.insert("admin_keys".into(), scabbard_admin_keys);

    let mut roster = vec![];
    for node in members.iter() {
        let peer_services = match serde_json::to_string(
            &members
                .iter()
                .filter_map(|other_node| {
                    if other_node.node_id != node.node_id {
                        Some(format!("gameroom_{}", other_node.node_id))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
        ) {
            Ok(s) => s,
            Err(err) => {
                return HttpResponse::InternalServerError()
                    .json(format!("failed to serialize peer services: {}", err))
                    .into_future()
            }
        };

        let mut service_args = scabbard_args.clone();
        service_args.insert("peer_services".into(), peer_services);

        roster.push(SplinterService {
            service_id: format!("gameroom_{}", node.node_id),
            service_type: "scabbard".to_string(),
            allowed_nodes: vec![node.node_id.to_string()],
            arguments: service_args,
        });
    }

    let create_request = CreateCircuit {
        circuit_id: format!(
            "gameroom{}::{}",
            partial_circuit_id,
            Uuid::new_v4().to_string()
        ),
        roster,
        members,
        authorization_type: AuthorizationType::Trust,
        persistence: PersistenceType::Any,
        durability: DurabilityType::NoDurabilty,
        routes: RouteType::Any,
        circuit_management_type: "gameroom".to_string(),
        application_metadata,
    };

    let payload_bytes = match make_payload(create_request) {
        Ok(bytes) => bytes,
        Err(err) => {
            debug!("Failed to make circuit management payload: {}", err);
            return HttpResponse::InternalServerError().finish().into_future();
        }
    };

    HttpResponse::Ok()
        .json(json!({ "data": { "payload_bytes": payload_bytes } }))
        .into_future()
}

fn make_application_metadata(alias: &str) -> Result<Vec<u8>, RestApiResponseError> {
    serde_json::to_vec(&ApplicationMetadata {
        alias: alias.to_string(),
    })
    .map_err(|err| RestApiResponseError::InternalError(err.to_string()))
}

fn make_payload(create_request: CreateCircuit) -> Result<Vec<u8>, RestApiResponseError> {
    let circuit_proto = create_request.into_proto()?;
    let circuit_bytes = circuit_proto.write_to_bytes()?;
    let hashed_bytes = hash(MessageDigest::sha512(), &circuit_bytes)?;

    let mut header = Header::new();
    header.set_action(Action::CIRCUIT_CREATE_REQUEST);
    header.set_payload_sha512(hashed_bytes.to_vec());
    let header_bytes = header.write_to_bytes()?;

    let mut circuit_management_payload = CircuitManagementPayload::new();
    circuit_management_payload.set_header(header_bytes);
    circuit_management_payload.set_circuit_create_request(circuit_proto);
    let payload_bytes = circuit_management_payload.write_to_bytes()?;
    Ok(payload_bytes)
}
