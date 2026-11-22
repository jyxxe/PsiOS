#!/usr/bin/env python3
"""Generate the v1 documentation contract fixtures; this is not a PsiOS service.

The fixture digest helper deliberately accepts a small, exact JCS subset only.
It must not be copied as a general RFC 8785 implementation.
"""
from __future__ import annotations
import copy
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = ROOT / 'schemas'
EXAMPLES = ROOT / 'examples'
BASE = 'https://schemas.psios.example/v1/'
DRAFT = 'https://json-schema.org/draft/2020-12/schema'
VERSION = {'const': '1.0.0'}
S = {'type': 'string', 'minLength': 1}
ID = {'type': 'string', 'minLength': 1, 'maxLength': 256,
      'pattern': r'^[^\u0000-\u001f\u007f]+$'}
U64 = {'type': 'string', 'pattern': r'^(0|[1-9][0-9]{0,19})$',
       'description': 'Canonical unsigned decimal string. Semantic bound: <= 18446744073709551615.'}
POS = {'type': 'string', 'pattern': r'^[1-9][0-9]{0,19}$',
       'description': 'Canonical positive decimal string. Semantic u64 bound applies.'}
DIGEST = {'type': 'string', 'pattern': r'^sha256:[0-9a-f]{64}$'}
TIME = {'type': 'string', 'format': 'date-time',
        'pattern': r'^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$'}
BOOL = {'type': 'boolean'}
OUTCOME = {'enum': ['NotStarted', 'InFlight', 'Known', 'Unknown']}
COMP = {'enum': ['NotRequired', 'Planned', 'Running', 'Complete', 'Partial', 'Failed']}
TASK_STATES = ['Accepted', 'Queued', 'Running', 'WaitingInput', 'WaitingApproval',
               'WaitingResource', 'Paused', 'Cancelling', 'Succeeded', 'Failed', 'Cancelled']
RUN_STATES = ['Created', 'Running', 'WaitingInput', 'WaitingApproval', 'WaitingResource',
              'Paused', 'Cancelling', 'Succeeded', 'Failed', 'Cancelled', 'Lost']
WORKFLOW_STATES = ['Draft', 'Ready', 'Running', 'WaitingApproval', 'WaitingInput', 'Blocked',
                   'Cancelling', 'Compensating', 'Succeeded', 'Failed', 'Cancelled']
OP_STATES = ['Pending', 'WaitingApproval', 'WaitingDependency', 'Running', 'CancelRequested',
             'Succeeded', 'Failed', 'Cancelled']


def obj(properties, required=None, **extra):
    result = {'type': 'object', 'properties': properties,
              'required': list(properties) if required is None else required,
              'additionalProperties': False}
    result.update(extra)
    return result


def arr(item, minimum=0, maximum=4096, unique=False):
    return {'type': 'array', 'items': item, 'minItems': minimum,
            'maxItems': maximum, **({'uniqueItems': True} if unique else {})}


def ref(name):
    return {'$ref': BASE + name + '.schema.json'}


def common(name):
    return {'$ref': BASE + 'common.schema.json#/$defs/' + name}


def when(name, value, then):
    return {'if': {'properties': {name: {'const': value}}, 'required': [name]}, 'then': then}


def success_constraint():
    return when('lifecycle', 'Succeeded', {'properties': {'outcome_knowledge': {'const': 'Known'}}})


def unknown_condition_constraint():
    return when('outcome_knowledge','Unknown',{'properties':{'conditions':{
        'contains':{'type':'object','properties':{'type':{'const':'OutcomeUnknown'},'status':{'const':'True'}},
                    'required':['type','status']},'minContains':1}}})


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + '\n', encoding='utf-8')


schemas = {}


def register(name, title, body):
    schemas[name] = {'$schema': DRAFT, '$id': BASE + name + '.schema.json',
                     'title': title, **body}


register('common', 'PsiOS v1 shared types (not a writable generic API)', {
    '$defs': {
        'Id': ID, 'UInt64': U64, 'PositiveUInt64': POS, 'Digest': DIGEST,
        'Timestamp': TIME, 'OpaqueVersion': ID, 'OutcomeKnowledge': OUTCOME,
        'CompensationStatus': COMP,
        'ResourceRef': obj({'resource_id': ID, 'version': ID, 'digest': DIGEST}),
        'ContentRef': obj({'kind': {'const': 'ref'}, 'resource_id': ID, 'version': ID,
                           'digest': DIGEST, 'size_bytes': common('UInt64'), 'media_type': S}),
        'DurabilityProfile': obj({
            'profile_ref': common('ResourceRef'), 'ack_level': {'const': 'Durable'},
            'fault_scope': arr({'enum': ['ProcessCrash', 'PowerLoss', 'DeviceLoss', 'NodeLoss']}, 1, 4, True),
            'rpo_seconds': common('UInt64')}),
        'OriginRef': obj({
            'authority_id': ID, 'origin_type': {'enum': ['WorkflowRun', 'Schedule', 'ChatDispatch', 'HumanRequest', 'Service']},
            'origin_id': ID, 'step_key': ID, 'attempt_generation': common('UInt64')},
            ['authority_id', 'origin_type', 'origin_id'],
            allOf=[when('origin_type', 'WorkflowRun', {'required': ['step_key', 'attempt_generation']})]),
        'Precondition': obj({
            'resource_ref': common('ResourceRef'),
            'kind': {'enum': ['ResourceVersion', 'ContentDigest', 'CommitSHA', 'DeviceIdentity', 'ProfilePredicate']},
            'expected': S, 'enforcement': {'enum': ['ConditionalApply', 'ExclusiveAuthority']},
            'predicate_profile_ref': common('ResourceRef')},
            ['resource_ref', 'kind', 'expected', 'enforcement'],
            allOf=[when('kind', 'ProfilePredicate', {'required': ['predicate_profile_ref']})]),
        'Condition': obj({'type': ID, 'status': {'enum': ['True', 'False', 'Unknown']},
                          'reason': ID, 'message': {'type': 'string'},
                          'observed_generation': common('UInt64'), 'last_transition_at': TIME}),
        'Fence': obj({'resource_id': ID, 'recovery_incarnation': ID, 'epoch': common('UInt64')}),
        'RetentionRef': obj({'retention_id': ID, 'resource_ref': common('ResourceRef'),
                             'authority_id': ID, 'valid_until': TIME}),
    }
})

register('resource-envelope', 'Read envelope; kind-specific spec validation is mandatory', obj({
    'api_version': {'const': 'psios.v1'}, 'schema_version': VERSION, 'kind': ID,
    'metadata': obj({'resource_id': ID, 'identity_domain_id': ID, 'authority_id': ID,
                     'security_domain_id': ID, 'owner_principal_id': ID, 'resource_version': ID,
                     'spec_generation': common('UInt64'), 'created_at': TIME, 'updated_at': TIME}),
    'spec': {'type': 'object', 'description': 'Uninterpreted in this base envelope; must also pass registered kind schema before a write.'},
    'status': obj({'observed_generation': common('UInt64'), 'lifecycle': ID,
                    'conditions': arr(common('Condition'), maximum=64)})
}))

error_codes = ['InvalidArgument', 'SchemaVersionUnsupported', 'AuthenticationRequired', 'PermissionDenied',
    'ApprovalRequired', 'ApprovalInvalidated', 'ApprovalAlreadyBound', 'NotFound', 'Conflict',
    'PreconditionFailed', 'IdempotencyConflict', 'IdempotencyKeyExpired', 'QueueFull', 'QuotaExceeded',
    'ServiceNotInstalled', 'UnsupportedCapability', 'DependencyUnavailable', 'StorageUnavailable', 'PartialFailure', 'CursorExpired',
    'ResyncRequired', 'LeaseLost', 'Fenced', 'RecoveryRequired', 'OutcomeUnknown',
    'ResultEvicted', 'ResultUnavailable', 'Cancelled', 'DeadlineExceeded', 'InternalError']
register('error', 'Stable machine-readable error', obj({
    'schema_version': VERSION, 'code': {'enum': error_codes}, 'message': S, 'request_id': ID,
    'retryable': BOOL, 'retry_condition': {'enum': ['Never', 'SameRequestAfterBackoff', 'Reauthenticate',
        'Replan', 'ReconcileOutcome', 'Resynchronize', 'DependencyReady']},
    'operation_id': ID, 'details': {'type': 'object', 'description': 'Bounded, redacted diagnostics; never authorization inputs.'}},
    ['schema_version', 'code', 'message', 'request_id', 'retryable', 'retry_condition'],
    allOf=[when('retryable', False, {'properties': {'retry_condition': {'enum': ['Never', 'Reauthenticate', 'Replan', 'ReconcileOutcome', 'Resynchronize', 'DependencyReady']}}}),
           when('retryable', True, {'properties': {'retry_condition': {'enum': ['SameRequestAfterBackoff','DependencyReady']}}})]))

register('operation-plan', 'Immutable OperationPlan', obj({
    'schema_version': VERSION, 'plan_id': ID, 'purpose_ref': common('ResourceRef'),
    'requester_principal_id': ID, 'delegation_ref': common('ResourceRef'),
    'allowed_effects': arr(obj({'resource_ref': common('ResourceRef'), 'action': ID,
                               'scope_ref': common('ResourceRef')}), 1, 256),
    'input_refs': arr(common('ResourceRef')), 'runtime_refs': arr(common('ResourceRef'), 1, 64),
    'preconditions': arr(common('Precondition'), maximum=256), 'budget_ref': common('ResourceRef'),
    'deadline_at': TIME, 'data_policy_ref': common('ResourceRef'),
    'permission_delta_refs': arr(common('ResourceRef'), maximum=256),
    'recovery_plan_ref': common('ResourceRef'), 'irreversible_effects': arr(S, maximum=256),
    'use_scope': {'enum': ['SingleOperation', 'BoundedSteps']}, 'step_keys': arr(ID, 1, 256, True),
    'digest_profile': obj({'digest_algorithm': {'const': 'SHA-256'},
                            'canonicalization_version': {'const': 'RFC8785'},
                            'domain': {'const': 'psios/operation-plan/1'}}), 'plan_digest': DIGEST},
    ['schema_version','plan_id','purpose_ref','requester_principal_id','delegation_ref','allowed_effects',
     'input_refs','runtime_refs','preconditions','budget_ref','deadline_at','data_policy_ref',
     'permission_delta_refs','recovery_plan_ref','irreversible_effects','use_scope','digest_profile','plan_digest'],
    allOf=[when('use_scope','BoundedSteps',{'required':['step_keys']}),
           when('use_scope','SingleOperation',{'not':{'required':['step_keys']}})]))

register('operation', 'Operation lifecycle and effect knowledge', obj({
    'schema_version': VERSION, 'operation_id': ID, 'operation_revision': ID,
    'plan_ref': common('ResourceRef'), 'task_ref': common('ResourceRef'),
    'lifecycle': {'enum': OP_STATES}, 'outcome_knowledge': OUTCOME, 'compensation_status': COMP,
    'effect_refs': arr(common('ResourceRef')), 'conditions': arr(common('Condition'), maximum=64),
    'created_at': TIME, 'updated_at': TIME},
    ['schema_version','operation_id','operation_revision','plan_ref','lifecycle','outcome_knowledge',
     'compensation_status','effect_refs','conditions','created_at','updated_at'],
    allOf=[success_constraint(),unknown_condition_constraint()]))

register('effect-record', 'One logical resource or external effect', obj({
    'schema_version': VERSION, 'effect_id': ID, 'operation_id': ID, 'resource_ref': common('ResourceRef'),
    'action': ID, 'phase': {'enum': ['IntentRecorded','PermitBound','EffectStarted','EffectConfirmed','EffectReconciled']},
    'outcome_knowledge': OUTCOME, 'external_idempotency_key': ID, 'external_object_ref': S,
    'evidence_refs': arr(common('ResourceRef')), 'compensation_operation_ref': common('ResourceRef')},
    ['schema_version','effect_id','operation_id','resource_ref','action','phase','outcome_knowledge','evidence_refs'],
    allOf=[when('phase','EffectConfirmed',{'properties':{'outcome_knowledge':{'const':'Known'}},
                                             'required':['evidence_refs']}),
           when('phase','EffectReconciled',{'properties':{'outcome_knowledge':{'const':'Known'}}})]))

register('approval-request', 'Request for an authority decision', obj({
    'schema_version': VERSION, 'approval_request_id': ID, 'plan_ref': common('ResourceRef'),
    'plan_digest': DIGEST, 'requester_principal_id': ID, 'approver_policy_ref': common('ResourceRef'),
    'status': {'enum': ['Pending','Approved','Rejected','Cancelled','Expired','Superseded']},
    'expires_at': TIME, 'decision_ref': common('ResourceRef')},
    ['schema_version','approval_request_id','plan_ref','plan_digest','requester_principal_id','approver_policy_ref','status','expires_at'],
    allOf=[when('status', s, {'required':['decision_ref']}) for s in ['Approved','Rejected']]))

register('approval-decision', 'Immutable approval/rejection fact', obj({
    'schema_version': VERSION, 'decision_id': ID, 'request_ref': common('ResourceRef'),
    'plan_digest': DIGEST, 'decision': {'enum': ['Approved','Rejected']},
    'approver_principal_id': ID, 'decided_at': TIME, 'valid_until': TIME,
    'approval_policy_ref': common('ResourceRef'), 'preconditions': arr(common('Precondition'), maximum=256),
    'use_limit': {'enum': ['SingleOperation','BoundedSteps']}, 'step_keys': arr(ID, 1, 256, True),
    'verification_evidence_ref': common('ResourceRef')},
    ['schema_version','decision_id','request_ref','plan_digest','decision','approver_principal_id',
     'decided_at','valid_until','approval_policy_ref','preconditions','use_limit','verification_evidence_ref'],
    allOf=[when('use_limit','BoundedSteps',{'required':['step_keys']}),
           when('use_limit','SingleOperation',{'not':{'required':['step_keys']}})]))

register('approval-use', 'CAS-bound approval usage ledger view', obj({
    'schema_version': VERSION, 'approval_use_id': ID, 'decision_ref': common('ResourceRef'),
    'resource_version': ID, 'plan_digest': DIGEST,
    'binding_status': {'enum': ['Unbound','Bound']},
    'validity_status': {'enum': ['Valid','Revoked','Expired','Superseded']},
    'operation_id': ID, 'bound_at': TIME, 'approved_step_key': ID},
    ['schema_version','approval_use_id','decision_ref','resource_version','plan_digest','binding_status','validity_status'],
    allOf=[when('binding_status','Bound',{'required':['operation_id','bound_at']}),
           when('binding_status','Unbound',{'not':{'anyOf':[{'required':['operation_id']},{'required':['bound_at']}]}})]))

register('execution-permit', 'Authenticated execution admission; JSON alone is not proof', obj({
    'schema_version': VERSION, 'permit_id': ID, 'operation_id': ID, 'effect_id': ID, 'plan_digest': DIGEST,
    'audience_resource_refs': arr(common('ResourceRef'), 1, 256), 'principal_chain_digest': DIGEST,
    'authorization_epoch': common('UInt64'), 'security_recovery_epoch': common('UInt64'),
    'recovery_incarnation': ID, 'authorization_ref': common('ResourceRef'),
    'decision_ref': common('ResourceRef'), 'preconditions': arr(common('Precondition'), maximum=256),
    'issued_at': TIME, 'not_after': TIME, 'usage': {'const':'SingleLogicalEffect'},
    'auth_proof': {'oneOf': [
        obj({'mode':{'const':'AuthorityLookup'},'authority_id':ID,'record_ref':common('ResourceRef')}),
        obj({'mode':{'const':'Signed'},'algorithm':ID,'key_id':ID,
             'signature':{'type':'string','pattern':r'^[A-Za-z0-9_-]+$'}})]}},
    ['schema_version','permit_id','operation_id','effect_id','plan_digest','audience_resource_refs',
     'principal_chain_digest','authorization_epoch','security_recovery_epoch','recovery_incarnation',
     'authorization_ref','preconditions','issued_at','not_after','usage','auth_proof']))

register('task-submit', 'Task.Submit request: no server-generated Task or Run ID', obj({
    'schema_version': VERSION, 'origin_ref': common('OriginRef'), 'idempotency_key': ID,
    'request_digest': DIGEST, 'input_ref': common('ResourceRef'), 'task_type': ID,
    'delegation_ref': common('ResourceRef'), 'budget_ref': common('ResourceRef'),
    'deadline_at': TIME, 'cancel_scope_id': ID},
    ['schema_version','origin_ref','idempotency_key','request_digest','input_ref','task_type',
     'delegation_ref','budget_ref','cancel_scope_id']))

register('task-accepted', 'Task persistent acceptance receipt', obj({
    'schema_version': VERSION, 'task_id': ID, 'task_revision': ID, 'accepted_at': TIME,
    'request_digest': DIGEST, 'origin_ref': common('OriginRef'), 'idempotency_key': ID,
    'deduplicated': BOOL, 'durability_profile': common('DurabilityProfile')}))

register('task-lookup', 'Authoritative submission lookup; absence has bounded meaning', obj({
    'schema_version': VERSION, 'status': {'enum':['Found','AbsentWithinWindow','Expired','Indeterminate']},
    'authority_id': ID, 'recovery_incarnation': ID, 'checked_at': TIME,
    'origin_ref': common('OriginRef'), 'idempotency_key': ID, 'request_digest': DIGEST,
    'namespace_ref': common('ResourceRef'), 'accepted': ref('task-accepted'),
    'window_not_after': TIME, 'consistency_evidence_ref': common('ResourceRef'), 'reason': S},
    ['schema_version','status','authority_id','recovery_incarnation','checked_at','origin_ref',
     'idempotency_key','request_digest','namespace_ref'],
    allOf=[when('status','Found',{'required':['accepted'],'not':{'required':['window_not_after']}}),
           when('status','AbsentWithinWindow',{'required':['window_not_after','consistency_evidence_ref'],
                                                'not':{'required':['accepted']}}),
           *[when('status',s,{'required':['reason'],'not':{'anyOf':[{'required':['accepted']},{'required':['window_not_after']}]}})
             for s in ['Expired','Indeterminate']]]))

result_schema = obj({'availability':{'enum':['Pending','NoneExpected','Available','Evicted','Unavailable']},
                     'artifact_refs':arr(common('ResourceRef'))}, allOf=[
    when('availability','Available',{'properties':{'artifact_refs':{'minItems':1}}}),
    when('availability','NoneExpected',{'properties':{'artifact_refs':{'maxItems':0}}})])
register('task', 'Task read view; terminal result availability is independent', obj({
    'schema_version': VERSION,'task_id':ID,'task_revision':ID,'origin_ref':common('OriginRef'),
    'request_digest':DIGEST,'lifecycle':{'enum':TASK_STATES},'outcome_knowledge':OUTCOME,
    'cancel_scope_id':ID,'current_run_id':ID,'retry_of_task_id':ID,'result':result_schema,
    'operation_refs':arr(common('ResourceRef')),'conditions':arr(common('Condition'),maximum=64)},
    ['schema_version','task_id','task_revision','origin_ref','request_digest','lifecycle',
     'outcome_knowledge','cancel_scope_id','result','operation_refs','conditions'],
    allOf=[success_constraint(),unknown_condition_constraint(),
           when('lifecycle','Succeeded',{'properties':{'result':{'properties':{'availability':{'enum':['NoneExpected','Available','Evicted','Unavailable']}}}}}),
           *[when('lifecycle',s,{'properties':{'conditions':{'minItems':1}}})
             for s in ['WaitingInput','WaitingApproval','WaitingResource','Paused']]]))

register('run', 'One Task execution attempt', obj({
    'schema_version': VERSION,'run_id':ID,'task_id':ID,'claim_id':ID,
    'claim_epoch':common('UInt64'),'recovery_incarnation':ID,'lifecycle':{'enum':RUN_STATES},
    'outcome_knowledge':OUTCOME,'input_ref':common('ResourceRef'),
    'execution_snapshot_ref':common('ResourceRef'),
    'output_refs':arr(common('ResourceRef')),'operation_refs':arr(common('ResourceRef')),
    'conditions':arr(common('Condition'),maximum=64)},
    allOf=[success_constraint(),unknown_condition_constraint()]))

register('cancel-origin', 'CancelOrigin is serialized with submission acceptance', obj({
    'schema_version':VERSION,'origin_ref':common('OriginRef'),'idempotency_key':ID,
    'request_digest':DIGEST,'cancel_scope_id':ID,'cancel_request_id':ID,'reason':S}))

parts = {'oneOf':[
    obj({'kind':{'const':'text'},'text':{'type':'string','maxLength':65536}}),
    obj({'kind':{'const':'json'},'schema_ref':common('ResourceRef'),'data':{}}),
    obj({'kind':{'const':'asset_ref'},'asset_ref':common('ResourceRef')})]}
chat_props = {'schema_version':VERSION,'conversation_id':ID,'client_send_id':ID,'content_digest':DIGEST,
    'content':obj({'parts':arr(parts,1,256)}),'thread_id':ID,'reply_to_message_id':ID,
    'mentions':arr(ID,maximum=256,unique=True),
    'message_type':{'enum':['message','request','response','decision','approval_request','task_event']},
    'attachment_refs':arr(common('ResourceRef'),maximum=256),'correlation_id':ID,'causation_id':ID,
    'task_ref':common('ResourceRef')}
chat_required = ['schema_version','conversation_id','client_send_id','content_digest','content',
                 'mentions','message_type','attachment_refs']
register('chat-send', 'Client SendMessage; sender is derived from authentication', obj(chat_props,chat_required))
chat_receipt = {'schema_version':VERSION,'message_id':ID,'conversation_id':ID,'client_send_id':ID,
    'conversation_seq':common('UInt64'),'writer_epoch':common('UInt64'),
    'membership_revision':common('UInt64'),'recovery_incarnation':ID,'accepted_at':TIME,
    'content_digest':DIGEST,'deduplicated':BOOL,'durability_profile':common('DurabilityProfile')}
register('chat-accepted', 'Persistent Conversation acceptance receipt', obj(chat_receipt))
register('message', 'Persistent Message read view; revision and initial send digest are separate', obj({
    **{k:v for k,v in chat_props.items() if k not in ['content','content_digest']},
    **{k:v for k,v in chat_receipt.items() if k not in ['deduplicated','content_digest']},
    'sender_principal_id':ID,'message_revision':ID,'initial_send_digest':DIGEST,
    'current_content_ref':common('ResourceRef'),
    'content_state':{'enum':['Visible','Redacted','Deleted','Unavailable']},
    'retention_refs':arr(common('RetentionRef'),maximum=256)},
    ['schema_version','message_id','conversation_id','client_send_id','conversation_seq','writer_epoch',
     'membership_revision','recovery_incarnation','accepted_at','durability_profile','sender_principal_id',
     'message_revision','initial_send_digest','content_state','mentions','message_type','attachment_refs','retention_refs'],
    allOf=[when('content_state','Visible',{'required':['current_content_ref']})]))

expression = {'oneOf':[
    obj({'kind':{'const':'literal'},'value':{}}),
    obj({'kind':{'const':'input'},'path':arr({'oneOf':[{'type':'string'},{'type':'integer','minimum':0,'maximum':1000000}]},maximum=32)}),
    obj({'kind':{'const':'call'},'function':{'enum':['eq','lt','lte','gt','gte','and','or','not','exists','type','array_contains']},
         'args':arr({'$ref':'#/$defs/Expression'},1,32)})]}
retry = obj({'max_attempts':common('PositiveUInt64'),'backoff_seconds':common('UInt64'),
             'retry_unknown_effects':{'const':False}})
node_common = {'step_id':ID,'depends_on':arr(ID,maximum=4096,unique=True),
               'deadline_seconds':common('PositiveUInt64'),'retry_policy':retry,
               'failure_policy':{'enum':['Stop','ContinueTyped']},
               'compensation_definition_ref':common('ResourceRef')}
node_required = ['step_id','depends_on','deadline_seconds','retry_policy','failure_policy']
bindings = {'type':'object','additionalProperties':{'$ref':'#/$defs/Expression'},'maxProperties':256}
def node(kind, props, required=None):
    return obj({**node_common,'kind':{'const':kind},**props},
               node_required + ['kind'] + (list(props) if required is None else required))
nodes = [
    node('TaskStep',{'task_type':ID,'input_bindings':bindings,
                    'allowed_capability_refs':arr(common('ResourceRef'),1,256),
                    'delegation_ref':common('ResourceRef'),'budget_ref':common('ResourceRef')}),
    node('Condition',{'expression':{'$ref':'#/$defs/Expression'},
                      'branches':arr(obj({'value':{},'activate':arr(ID,1,256,True)}),1,64),
                      'default_activate':arr(ID,maximum=256,unique=True)}),
    node('Parallel',{'children':arr(ID,1,256,True),'max_concurrency':common('PositiveUInt64')}),
    node('Join',{'strategy':{'enum':['AllSuccess','AllSettled','AnySuccess']},
                'predecessors':arr(ID,1,256,True),'nonselected_policy':{'const':'Exclude'},
                'loser_policy':{'enum':['ReconcileBeforeRelease','NotApplicable']},
                'aggregate_schema_ref':common('ResourceRef')}),
    node('ApprovalGate',{'operation_plan_ref':common('ResourceRef'),
                         'approval_policy_ref':common('ResourceRef'),
                         'on_rejected':{'enum':['Fail','Cancel']}}),
    node('WaitInput',{'allowed_principal_ids':arr(ID,1,256,True),'input_schema_ref':common('ResourceRef')}),
    node('WaitEvent',{'subscription_ref':common('ResourceRef'),'event_type':ID,
                      'predicate':{'$ref':'#/$defs/Expression'}}),
    node('BoundedLoop',{'body_definition_ref':common('ResourceRef'),'max_iterations':common('PositiveUInt64'),
                        'while_expression':{'$ref':'#/$defs/Expression'},'budget_ref':common('ResourceRef')}),
    node('SubWorkflow',{'child_definition_ref':common('ResourceRef'),'input_bindings':bindings,
                        'budget_ref':common('ResourceRef')}),
    node('Map',{'child_definition_ref':common('ResourceRef'),'items_expression':{'$ref':'#/$defs/Expression'},
                'max_items':common('PositiveUInt64'),'max_concurrency':common('PositiveUInt64'),
                'expansion_policy':{'const':'FreezeAtExpansion'},'budget_ref':common('ResourceRef')})]
register('workflow-definition','Restricted Workflow AST (graph/authorization checks are separate)',obj({
    'schema_version':VERSION,'dsl_version':{'const':'1.0.0'},'definition_id':ID,'definition_version':ID,
    'definition_digest':DIGEST,'input_schema_ref':common('ResourceRef'),'output_schema_ref':common('ResourceRef'),
    'nodes':arr({'oneOf':nodes},1,4096),'root_budget_ref':common('ResourceRef'),
    'max_concurrency':common('PositiveUInt64'),'deadline_seconds':common('PositiveUInt64'),
    'output_bindings':bindings}, **{'$defs':{'Expression':expression}}))

register('workflow-run','WorkflowRun state with immutable definition/input refs',obj({
    'schema_version':VERSION,'workflow_run_id':ID,'resource_version':ID,
    'definition_ref':common('ResourceRef'),'input_ref':common('ResourceRef'),
    'lifecycle':{'enum':WORKFLOW_STATES},'outcome_knowledge':OUTCOME,'compensation_status':COMP,
    'cancel_scope_id':ID,'root_budget_ref':common('ResourceRef'),'step_instance_refs':arr(common('ResourceRef')),
    'output_refs':arr(common('ResourceRef')),'conditions':arr(common('Condition'),maximum=64)},
    allOf=[success_constraint(),unknown_condition_constraint(),
           *[when('lifecycle',s,{'properties':{'conditions':{'minItems':1}}})
             for s in ['WaitingInput','WaitingApproval','Blocked']]]))

register('dispatch-intent','Workflow-to-Task durable dispatch intent',obj({
    'schema_version':VERSION,'intent_id':ID,'workflow_run_id':ID,'step_key':ID,
    'attempt_generation':common('UInt64'),'submit_request':ref('task-submit'),
    'status':{'enum':['Pending','Submitted','Linked','CancelPending','Cancelled','Blocked']},
    'accepted':ref('task-accepted'),'cancel_request_ref':common('ResourceRef'),'reason':S},
    ['schema_version','intent_id','workflow_run_id','step_key','attempt_generation','submit_request','status'],
    allOf=[when('status','Linked',{'required':['accepted']}),
           when('status','CancelPending',{'required':['cancel_request_ref']}),
           when('status','Blocked',{'required':['reason']})]))

register('event','Committed fact notification envelope, not an authorization token',obj({
    'schema_version':VERSION,'event_id':ID,'event_type':ID,'source_authority_id':ID,
    'recovery_incarnation':ID,'source_seq':common('UInt64'),'resource_ref':common('ResourceRef'),
    'occurred_at':TIME,'received_at':TIME,'correlation_id':ID,'causation_id':ID,
    'task_id':ID,'run_id':ID,'payload_ref':common('ResourceRef'),'authorization_context_ref':common('ResourceRef')},
    ['schema_version','event_id','event_type','source_authority_id','recovery_incarnation',
     'source_seq','resource_ref','occurred_at','received_at','correlation_id','payload_ref','authorization_context_ref']))

record_fields = obj({'uid':ID,'name':{'type':'string'},'content':{'oneOf':[{'type':'string'},common('ContentRef')]},
    'tag':arr({'type':'string'},maximum=4096,unique=True),'score':{'type':['number','null']},
    'vector':arr({'type':'number'},1,65536),'extra_data':{}},['uid','content','tag','score'])
register('dataset-record','Dataset record: seven business fields and independent system metadata',obj({
    'schema_version':VERSION,'dataset_id':ID,'record':record_fields,
    'metadata':obj({'record_version':ID,'content_version':ID,'content_digest':DIGEST,
        'score_profile_ref':common('ResourceRef'),'score_source_content_version':ID,
        'embedding_profile':obj({'profile_ref':common('ResourceRef'),'model_id':ID,'model_version':ID,
            'dimensions':common('PositiveUInt64'),'distance':{'enum':['Cosine','Dot','Euclidean']},
            'normalization':{'enum':['None','UnitL2']},'source_content_version':ID}),
        'created_at':TIME,'updated_at':TIME},['record_version','content_version','content_digest','created_at','updated_at'])},
    allOf=[{'if':{'properties':{'record':{'required':['vector']}}},
            'then':{'properties':{'metadata':{'required':['embedding_profile']}}}},
           {'if':{'properties':{'record':{'properties':{'score':{'type':'number'}},'required':['score']}}},
            'then':{'properties':{'metadata':{'required':['score_profile_ref','score_source_content_version']}}}}]))

register('volume','Logical Volume; access_mode does not itself prove fencing',obj({
    'schema_version':VERSION,'volume_id':ID,'data_incarnation':ID,'resource_version':ID,
    'pool_id':ID,'driver_profile_ref':common('ResourceRef'),
    'access_mode':{'enum':['multi_reader','single_node_rw','single_writer_rw','multi_writer_rw']},
    'capacity_bytes':common('UInt64'),'lifecycle':{'enum':['Provisioning','Available','Deleting','Deleted']},
    'health':{'enum':['Unknown','Healthy','Degraded','Unavailable']},
    'attachment_refs':arr(common('ResourceRef')),'backup_policy_ref':common('ResourceRef')}))

register('volume-attachment','Attachment is separate from Volume lifecycle',obj({
    'schema_version':VERSION,'attachment_id':ID,'volume_id':ID,'data_incarnation':ID,
    'node_id':ID,'workload_id':ID,'requested_access':{'enum':['ReadOnly','ReadWrite']},
    'effective_access':{'enum':['Blocked','ReadOnly','ReadWrite']},
    'lifecycle':{'enum':['Attaching','Attached','Detaching','Detached']},
    'fence':common('Fence'),'lease_ref':common('ResourceRef')},
    ['schema_version','attachment_id','volume_id','data_incarnation','node_id','workload_id',
     'requested_access','effective_access','lifecycle'],
    allOf=[when('requested_access','ReadOnly',{'properties':{'effective_access':{'enum':['Blocked','ReadOnly']}}}),
           when('lifecycle','Detached',{'properties':{'effective_access':{'const':'Blocked'}}})]))

register('capability-descriptor','Advertised backend capability, never a Grant',obj({
    'schema_version':VERSION,'capability_id':ID,'capability_version':ID,'authority_id':ID,
    'actions':arr(ID,1,256,True),'profile_ref':common('ResourceRef'),
    'revocation_capabilities':arr({'enum':['GateOnly','StreamCut','SandboxTerminate','ResourceFence']},maximum=4,unique=True),
    'effect_limitations':arr(S,maximum=256),'availability':{'enum':['Ready','Degraded','Unavailable','NotInstalled']},
    'implementation_status':{'enum':['NotVerified','ImplementedUnverified','Verified']}}))

register('revocation-result','Per-resource enforcement and residual effects',obj({
    'schema_version':VERSION,'revocation_id':ID,'accepted_at':TIME,'authorization_epoch':common('UInt64'),
    'status':{'enum':['Accepted','InProgress','Complete','Partial','RecoveryRequired']},
    'targets':arr(obj({'resource_id':ID,'required':BOOL,
        'capabilities':arr({'enum':['GateOnly','StreamCut','SandboxTerminate','ResourceFence']},maximum=4,unique=True),
        'enforcement_status':{'enum':['Pending','Enforced','Unsupported','Unreachable','Failed']},
        'evidence_refs':arr(common('ResourceRef')),'residual_effects':arr(S,maximum=256)}),1,4096)}))

register('recovery-status','Recovery trust, effect reconciliation and publication barriers',obj({
    'schema_version':VERSION,'recovery_operation_id':ID,'trust_domain_id':ID,'security_domain_id':ID,
    'security_recovery_epoch':common('UInt64'),'recovery_incarnation':ID,
    'security_mode':{'enum':['Quarantined','VerifiedCurrent','ReEnrolled']},
    'covered_resource_ids':arr(ID,maximum=4096,unique=True),'uncovered_resource_ids':arr(ID,maximum=4096,unique=True),
    'security_evidence_refs':arr(common('ResourceRef')),
    'effect_reconciliation':{'enum':['Blocked','InProgress','Complete']},
    'unknown_effect_refs':arr(common('ResourceRef')),
    'deletion_history':{'enum':['Unknown','VerifiedWithinScope']},
    'automatic_writes_enabled':BOOL,'public_sharing_enabled':BOOL,'verified_at':TIME},
    ['schema_version','recovery_operation_id','trust_domain_id','security_domain_id','security_recovery_epoch',
     'recovery_incarnation','security_mode','covered_resource_ids','uncovered_resource_ids','security_evidence_refs',
     'effect_reconciliation','unknown_effect_refs','deletion_history','automatic_writes_enabled','public_sharing_enabled'],
    allOf=[when('security_mode','Quarantined',{'properties':{'automatic_writes_enabled':{'const':False},'public_sharing_enabled':{'const':False}}}),
           when('deletion_history','Unknown',{'properties':{'public_sharing_enabled':{'const':False}}}),
           when('automatic_writes_enabled',True,{'properties':{'effect_reconciliation':{'const':'Complete'},
                'unknown_effect_refs':{'maxItems':0},'security_mode':{'enum':['VerifiedCurrent','ReEnrolled']}}})]))

register('recovery-set-manifest','Recovery set with explicit consistency and external-effect scope',obj({
    'schema_version':VERSION,'recovery_set_id':ID,'source_authority_id':ID,'source_incarnation':ID,
    'created_at':TIME,'creator_tool_version':ID,'manifest_digest':DIGEST,
    'digest_profile':obj({'digest_algorithm':{'const':'SHA-256'},'canonicalization_version':{'const':'RFC8785'},
                         'domain':{'const':'psios/recovery-set-manifest/1'}}),
    'scope':{'enum':['single_volume','volume_group','service_group','node']},
    'capture_interval':obj({'started_at':TIME,'completed_at':TIME}),
    'consistency':{'enum':['crash_consistent','application_consistent']},
    'coordination':{'enum':['CoordinatedCut','IndependentCheckpointsWithReconciliation']},
    'consistency_evidence_refs':arr(common('ResourceRef')),
    'members':arr(obj({'resource_id':ID,'data_incarnation':ID,'schema_version':ID,'checkpoint':ID,
        'backup_ref':common('ResourceRef'),'size_bytes':common('UInt64')}),1,4096),
    'cross_service_relation_refs':arr(common('ResourceRef')),'incremental_dependencies':arr(common('ResourceRef')),
    'required_log_refs':arr(common('ResourceRef')),'security_evidence_refs':arr(common('ResourceRef')),
    'secret_refs':arr(common('ResourceRef')),'identity_mapping_ref':common('ResourceRef'),
    'effect_reconciliation_plan_ref':common('ResourceRef'),'replay_prohibited_refs':arr(common('ResourceRef')),
    'retention_policy_ref':common('ResourceRef'),'failure_domain':ID,
    'restore_procedure_ref':common('ResourceRef'),'independent_tool_ref':common('ResourceRef'),
    'required_restore_bytes':common('UInt64'),'last_drill_evidence_ref':common('ResourceRef')},
    ['schema_version','recovery_set_id','source_authority_id','source_incarnation','created_at','creator_tool_version',
     'manifest_digest','digest_profile','scope','capture_interval','consistency','coordination','consistency_evidence_refs',
     'members','cross_service_relation_refs','incremental_dependencies','required_log_refs','security_evidence_refs',
     'secret_refs','identity_mapping_ref','effect_reconciliation_plan_ref','replay_prohibited_refs',
     'retention_policy_ref','failure_domain','restore_procedure_ref','independent_tool_ref','required_restore_bytes']))

register('application-manifest','Application release requirements; an installed Deployment is separate',obj({
    'schema_version':VERSION,'application_id':ID,'release_id':ID,'release_version':ID,
    'artifact_ref':common('ResourceRef'),'publisher_identity_ref':common('ResourceRef'),
    'runtime_profile_ref':common('ResourceRef'),'release_spec_ref':common('ResourceRef'),
    'package_format':{'enum':['OCI','Flatpak','DEB','RPM','AlpineAPK','NativeWeb','HostExtension']},
    'required_capability_refs':arr(common('ResourceRef')),'volume_requirements':arr(obj({
        'name':ID,'minimum_bytes':common('UInt64'),'access_mode':{'enum':['multi_reader','single_node_rw','single_writer_rw','multi_writer_rw']},
        'schema_ref':common('ResourceRef')}),maximum=64),
    'network_policy_ref':common('ResourceRef'),'health_check_ref':common('ResourceRef'),
    'migration_plan_ref':common('ResourceRef'),'rollback_policy_ref':common('ResourceRef'),
    'compatibility_claims':arr(obj({'capability':{'enum':['Recognized','Imported','RuntimeVerified','GUIVerified']},
        'result':{'enum':['NotRun','Passed','Failed','Blocked','NotApplicable']},
        'profile_ref':common('ResourceRef'),'evidence_refs':arr(common('ResourceRef')),
        'limitations':arr(S,maximum=256),'not_applicable_reason':S},
        ['capability','result','profile_ref','evidence_refs','limitations'],
        allOf=[when('result','Passed',{'properties':{'evidence_refs':{'minItems':1}}}),
               when('result','NotApplicable',{'required':['not_applicable_reason']})]),1,4)}))

register('application-release-spec','Executable application requirements resolved by ApplicationManifest',obj({
    'schema_version':VERSION,'release_spec_id':ID,
    'execution':obj({'argv':arr(S,1,1024),'working_directory':{'type':'string','pattern':r'^/'},
        'environment':{'type':'object','maxProperties':512,
            'propertyNames':{'pattern':r'^[A-Za-z_][A-Za-z0-9_]*$'},
            'additionalProperties':{'oneOf':[{'type':'string','maxLength':65536},obj({'secret_ref':common('ResourceRef')})]}}}),
    'lifecycle':obj({'stop_signal':{'enum':['SIGTERM','SIGINT','SIGHUP']},
        'stop_grace_seconds':common('UInt64'),'start_deadline_seconds':common('PositiveUInt64'),
        'restart_policy':{'enum':['Never','OnFailure','Always']},'max_restarts':common('UInt64')}),
    'identity':obj({'execution_identity_profile_ref':common('ResourceRef'),
        'namespace_mapping_profile_ref':common('ResourceRef')}),
    'permission_requests':obj({'capability_refs':arr(common('ResourceRef'),maximum=256),
        'secret_refs':arr(common('ResourceRef'),maximum=256),'device_profile_refs':arr(common('ResourceRef'),maximum=256),
        'volume_bindings':arr(obj({'manifest_volume_name':ID,'mount_path':{'type':'string','pattern':r'^/'},
            'requested_access':{'enum':['ReadOnly','ReadWrite']}}),maximum=64)}),
    'resources':obj({'cpu_millicores':common('PositiveUInt64'),'memory_max_bytes':common('PositiveUInt64'),
        'pids_max':common('PositiveUInt64'),'ephemeral_max_bytes':common('UInt64'),
        'io_profile_ref':common('ResourceRef'),'gpu_profile_ref':common('ResourceRef')},
        ['cpu_millicores','memory_max_bytes','pids_max','ephemeral_max_bytes','io_profile_ref']),
    'provenance':obj({'source_refs':arr(common('ResourceRef'),1,256),'license_expression':S,
        'license_material_ref':common('ResourceRef'),
        'source_trust':{'enum':['PublisherVerified','RepositoryVerified','UserSuppliedUnverified']},
        'trust_evidence_or_admission_policy_ref':common('ResourceRef'),
        'build_record_ref':common('ResourceRef'),'sbom_ref':common('ResourceRef')}),
    'dependency_release_refs':arr(common('ResourceRef'),maximum=1024),
    'backup_scope_ref':common('ResourceRef')}))

register('snapshot-page','One page of a fixed authority snapshot',obj({
    'schema_version':VERSION,'authority_id':ID,'recovery_incarnation':ID,'snapshot_id':ID,
    'items':arr(common('ResourceRef')),'next_cursor':{'type':['string','null']},
    'watch_start_cursor':ID,'expires_at':TIME,'authorization_epoch':common('UInt64'),
    'complete':BOOL},allOf=[when('complete',True,{'properties':{'next_cursor':{'type':'null'}}}),
                          when('complete',False,{'properties':{'next_cursor':{'type':'string','minLength':1}}})]))

# Examples intentionally use fake stable IDs, immutable references and no secret values.
T = '2026-11-22T08:00:00Z'
LATER = '2026-11-22T09:00:00Z'
UNKNOWN_CONDITION={'type':'OutcomeUnknown','status':'True','reason':'ExternalReceiptUnavailable',
    'message':'Reconcile the previously issued effect before allowing new effects.',
    'observed_generation':'1','last_transition_at':T}
def rr(name, version='rev-1'):
    return {'resource_id':name,'version':version,'digest':'sha256:'+hashlib.sha256(name.encode()).hexdigest()}


def restricted_jcs(value):
    """Exact for fixture subset: ASCII keys, finite safe integer numbers only."""
    def check(v):
        if isinstance(v,dict):
            if any(not isinstance(k,str) or not k.isascii() for k in v):
                raise ValueError('Fixture helper accepts ASCII object keys only')
            for x in v.values(): check(x)
        elif isinstance(v,list):
            for x in v: check(x)
        elif isinstance(v,bool) or v is None: pass
        elif isinstance(v,int):
            if abs(v)>2**53-1: raise ValueError('Not a JCS-safe fixture integer')
        elif isinstance(v,str):
            v.encode('utf-8',errors='strict')
        else: raise ValueError('Use a conforming RFC8785 library for other numeric values')
    check(value)
    return json.dumps(value,ensure_ascii=False,sort_keys=True,separators=(',',':'),allow_nan=False).encode('utf-8')


def with_digest(value,field,contract):
    value=copy.deepcopy(value)
    value.pop(field,None)
    value[field]='sha256:'+hashlib.sha256(('psios/'+contract+'/1\n').encode()+restricted_jcs(value)).hexdigest()
    return value


durability={'profile_ref':rr('profile-local-durable'),'ack_level':'Durable',
            'fault_scope':['ProcessCrash','PowerLoss'],'rpo_seconds':'0'}
origin={'authority_id':'workflow-local','origin_type':'WorkflowRun','origin_id':'wf-run-demo',
        'step_key':'review/iteration-0','attempt_generation':'0'}
submit=with_digest({'schema_version':'1.0.0','origin_ref':origin,'idempotency_key':'submit-window-1/review-0',
    'input_ref':rr('review-input'),'task_type':'review.read_only.v1','delegation_ref':rr('delegation-review'),
    'budget_ref':rr('budget-review'),'deadline_at':LATER,'cancel_scope_id':'cancel-scope-demo'},'request_digest','task-submit')
accepted={'schema_version':'1.0.0','task_id':'task-demo','task_revision':'revision-accepted',
    'accepted_at':T,'request_digest':submit['request_digest'],'origin_ref':origin,
    'idempotency_key':submit['idempotency_key'],'deduplicated':False,'durability_profile':durability}
send=with_digest({'schema_version':'1.0.0','conversation_id':'room-demo','client_send_id':'send-window-1/message-001',
    'content':{'parts':[{'kind':'text','text':'请检查这份方案。此消息不构成系统授权。'}]},
    'mentions':['agent-reviewer'],'message_type':'request','attachment_refs':[rr('design-attachment')]},'content_digest','chat-send')
chat_ack={'schema_version':'1.0.0','message_id':'message-demo','conversation_id':'room-demo',
    'client_send_id':send['client_send_id'],'conversation_seq':'9007199254740993','writer_epoch':'12',
    'membership_revision':'7','recovery_incarnation':'conversation-incarnation-2','accepted_at':T,
    'content_digest':send['content_digest'],'deduplicated':False,'durability_profile':durability}
precondition={'resource_ref':rr('target-document'),'kind':'ContentDigest','expected':rr('target-document')['digest'],
              'enforcement':'ConditionalApply'}
plan=with_digest({'schema_version':'1.0.0','plan_id':'plan-demo','purpose_ref':rr('user-purpose'),
    'requester_principal_id':'principal-user','delegation_ref':rr('delegation-review'),
    'allowed_effects':[{'resource_ref':rr('target-document'),'action':'document.replace',
                       'scope_ref':rr('scope-one-document')}],
    'input_refs':[rr('replacement-content')],'runtime_refs':[rr('tool-document-editor')],
    'preconditions':[precondition],'budget_ref':rr('budget-one-edit'),'deadline_at':LATER,
    'data_policy_ref':rr('policy-local-only'),'permission_delta_refs':[],
    'recovery_plan_ref':rr('recover-document-version'),'irreversible_effects':[],
    'use_scope':'SingleOperation','digest_profile':{'digest_algorithm':'SHA-256',
        'canonicalization_version':'RFC8785','domain':'psios/operation-plan/1'}},'plan_digest','operation-plan')
policy={'max_attempts':'1','backoff_seconds':'0','retry_unknown_effects':False}
definition=with_digest({'schema_version':'1.0.0','dsl_version':'1.0.0','definition_id':'workflow-review',
    'definition_version':'definition-rev-1','input_schema_ref':rr('schema-review-input'),
    'output_schema_ref':rr('schema-review-output'),'nodes':[{
        'step_id':'review','kind':'TaskStep','depends_on':[],'deadline_seconds':'600',
        'retry_policy':policy,'failure_policy':'Stop','task_type':'review.read_only.v1',
        'input_bindings':{'document':{'kind':'input','path':['document']}},
        'allowed_capability_refs':[rr('capability-document-read')],
        'delegation_ref':rr('delegation-review'),'budget_ref':rr('budget-review')}],
    'root_budget_ref':rr('budget-root'),'max_concurrency':'1','deadline_seconds':'900',
    'output_bindings':{'report':{'kind':'input','path':['steps','review','report']}}},'definition_digest','workflow-definition')

positives={
 'operation-plan':plan,
 'operation':{'schema_version':'1.0.0','operation_id':'operation-demo','operation_revision':'rev-3',
    'plan_ref':rr('plan-demo'),'lifecycle':'Cancelled','outcome_knowledge':'Unknown',
    'compensation_status':'NotRequired','effect_refs':[rr('effect-demo')],'conditions':[UNKNOWN_CONDITION],
    'created_at':T,'updated_at':LATER},
 'effect-record':{'schema_version':'1.0.0','effect_id':'effect-demo','operation_id':'operation-demo',
    'resource_ref':rr('target-document'),'action':'document.replace','phase':'EffectStarted',
    'outcome_knowledge':'Unknown','evidence_refs':[]},
 'approval-request':{'schema_version':'1.0.0','approval_request_id':'approval-request-demo',
    'plan_ref':rr('plan-demo'),'plan_digest':plan['plan_digest'],'requester_principal_id':'principal-user',
    'approver_policy_ref':rr('approval-policy'),'status':'Pending','expires_at':LATER},
 'approval-decision':{'schema_version':'1.0.0','decision_id':'decision-demo','request_ref':rr('approval-request-demo'),
    'plan_digest':plan['plan_digest'],'decision':'Approved','approver_principal_id':'principal-reviewer',
    'decided_at':T,'valid_until':LATER,'approval_policy_ref':rr('approval-policy'),
    'preconditions':[precondition],'use_limit':'SingleOperation','verification_evidence_ref':rr('approval-audit')},
 'approval-use':{'schema_version':'1.0.0','approval_use_id':'approval-use-demo','decision_ref':rr('decision-demo'),
    'resource_version':'rv-bound','plan_digest':plan['plan_digest'],'binding_status':'Bound',
    'validity_status':'Valid','operation_id':'operation-demo','bound_at':T},
 'execution-permit':{'schema_version':'1.0.0','permit_id':'permit-demo','operation_id':'operation-demo','effect_id':'effect-demo',
    'plan_digest':plan['plan_digest'],'audience_resource_refs':[rr('target-document')],
    'principal_chain_digest':rr('principal-chain')['digest'],'authorization_epoch':'19',
    'security_recovery_epoch':'3','recovery_incarnation':'core-incarnation-2',
    'authorization_ref':rr('delegation-review'),'decision_ref':rr('decision-demo'),
    'preconditions':[precondition],'issued_at':T,'not_after':LATER,'usage':'SingleLogicalEffect',
    'auth_proof':{'mode':'AuthorityLookup','authority_id':'approval-authority','record_ref':rr('permit-demo')}},
 'task-submit':submit,'task-accepted':accepted,
 'task-lookup':{'schema_version':'1.0.0','status':'Found','authority_id':'task-broker','recovery_incarnation':'core-incarnation-2',
    'checked_at':T,'origin_ref':origin,'idempotency_key':submit['idempotency_key'],'request_digest':submit['request_digest'],
    'namespace_ref':rr('submit-window-1'),'accepted':accepted},
 'task':{'schema_version':'1.0.0','task_id':'task-demo','task_revision':'rev-5','origin_ref':origin,
    'request_digest':submit['request_digest'],'lifecycle':'Succeeded','outcome_knowledge':'Known',
    'cancel_scope_id':'cancel-scope-demo','result':{'availability':'Evicted','artifact_refs':[rr('expired-report')]},
    'operation_refs':[],'conditions':[]},
 'run':{'schema_version':'1.0.0','run_id':'run-demo','task_id':'task-demo','claim_id':'claim-demo','claim_epoch':'2',
    'recovery_incarnation':'core-incarnation-2','lifecycle':'Lost','outcome_knowledge':'Unknown',
    'input_ref':rr('review-input'),'execution_snapshot_ref':rr('execution-snapshot-demo'),
    'output_refs':[],'operation_refs':[rr('operation-demo')],'conditions':[UNKNOWN_CONDITION]},
 'cancel-origin':{'schema_version':'1.0.0','origin_ref':origin,'idempotency_key':submit['idempotency_key'],
    'request_digest':submit['request_digest'],'cancel_scope_id':'cancel-scope-demo','cancel_request_id':'cancel-request-demo',
    'reason':'User cancelled while submission acknowledgement was unavailable.'},
 'chat-send':send,'chat-accepted':chat_ack,
 'message':{'schema_version':'1.0.0','message_id':'message-demo','conversation_id':'room-demo',
    'client_send_id':send['client_send_id'],'conversation_seq':'9007199254740993','writer_epoch':'12',
    'membership_revision':'7','recovery_incarnation':'conversation-incarnation-2','accepted_at':T,
    'durability_profile':durability,'sender_principal_id':'principal-user','message_revision':'message-rev-2',
    'initial_send_digest':send['content_digest'],'current_content_ref':rr('message-content-rev-2'),
    'content_state':'Visible','mentions':['agent-reviewer'],'message_type':'request',
    'attachment_refs':[rr('design-attachment')],'retention_refs':[]},
 'workflow-definition':definition,
 'workflow-run':{'schema_version':'1.0.0','workflow_run_id':'wf-run-demo','resource_version':'wf-rv-4',
    'definition_ref':rr('workflow-review'),'input_ref':rr('review-input'),'lifecycle':'Blocked',
    'outcome_knowledge':'Unknown','compensation_status':'NotRequired','cancel_scope_id':'cancel-scope-demo',
    'root_budget_ref':rr('budget-root'),'step_instance_refs':[rr('step-review-0')],'output_refs':[],'conditions':[UNKNOWN_CONDITION]},
 'dispatch-intent':{'schema_version':'1.0.0','intent_id':'intent-review-0','workflow_run_id':'wf-run-demo',
    'step_key':origin['step_key'],'attempt_generation':'0','submit_request':submit,'status':'Linked','accepted':accepted},
 'event':{'schema_version':'1.0.0','event_id':'event-demo','event_type':'psios.task.updated.v1',
    'source_authority_id':'task-broker','recovery_incarnation':'core-incarnation-2','source_seq':'42',
    'resource_ref':rr('task-demo'),'occurred_at':T,'received_at':T,'correlation_id':'correlation-demo',
    'task_id':'task-demo','payload_ref':rr('task-event-payload'),'authorization_context_ref':rr('event-policy-context')},
 'dataset-record':{'schema_version':'1.0.0','dataset_id':'dataset-demo',
    'record':{'uid':'record-book','name':'示例书籍','content':'这是一个完整逻辑记录。','tag':['book'],
              'score':None,'extra_data':['保留任意JSON根值',{'source':'demo'}]},
    'metadata':{'record_version':'record-rev-1','content_version':'content-rev-1',
        'content_digest':'sha256:'+hashlib.sha256('这是一个完整逻辑记录。'.encode()).hexdigest(),'created_at':T,'updated_at':T}},
 'volume':{'schema_version':'1.0.0','volume_id':'volume-demo','data_incarnation':'volume-incarnation-2',
    'resource_version':'volume-rv-2','pool_id':'pool-demo','driver_profile_ref':rr('driver-local'),
    'access_mode':'single_writer_rw','capacity_bytes':'10737418240','lifecycle':'Available','health':'Healthy',
    'attachment_refs':[rr('attachment-demo')],'backup_policy_ref':rr('backup-policy')},
 'volume-attachment':{'schema_version':'1.0.0','attachment_id':'attachment-demo','volume_id':'volume-demo',
    'data_incarnation':'volume-incarnation-2','node_id':'node-demo','workload_id':'workload-demo',
    'requested_access':'ReadWrite','effective_access':'ReadWrite','lifecycle':'Attached',
    'fence':{'resource_id':'volume-demo','recovery_incarnation':'storage-incarnation-2','epoch':'13'},'lease_ref':rr('lease-demo')},
 'capability-descriptor':{'schema_version':'1.0.0','capability_id':'storage.write','capability_version':'1.0.0',
    'authority_id':'storage-local','actions':['volume.write'],'profile_ref':rr('storage-profile'),
    'revocation_capabilities':['GateOnly','ResourceFence'],'effect_limitations':['Already exported bytes cannot be recalled.'],
    'availability':'Ready','implementation_status':'NotVerified'},
 'revocation-result':{'schema_version':'1.0.0','revocation_id':'revocation-demo','accepted_at':T,'authorization_epoch':'20',
    'status':'Partial','targets':[{'resource_id':'volume-demo','required':True,'capabilities':['ResourceFence'],
        'enforcement_status':'Enforced','evidence_refs':[rr('fence-evidence')],'residual_effects':[]},
        {'resource_id':'worker-offline','required':True,'capabilities':['SandboxTerminate'],'enforcement_status':'Unreachable',
         'evidence_refs':[],'residual_effects':['A previously issued external request may still complete.']}]},
 'recovery-status':{'schema_version':'1.0.0','recovery_operation_id':'restore-demo','trust_domain_id':'trust-new',
    'security_domain_id':'tenant-demo','security_recovery_epoch':'3','recovery_incarnation':'core-incarnation-2',
    'security_mode':'ReEnrolled','covered_resource_ids':['node-demo'],'uncovered_resource_ids':['external-forge'],
    'security_evidence_refs':[rr('reenrollment-evidence')],'effect_reconciliation':'Blocked',
    'unknown_effect_refs':[rr('effect-demo')],'deletion_history':'Unknown','automatic_writes_enabled':False,
    'public_sharing_enabled':False,'verified_at':T},
 'recovery-set-manifest':{'schema_version':'1.0.0','recovery_set_id':'recovery-set-demo','source_authority_id':'node-demo',
    'source_incarnation':'core-incarnation-1','created_at':T,'creator_tool_version':'prototype-unverified',
    'manifest_digest':'sha256:'+'0'*64,
    'digest_profile':{'digest_algorithm':'SHA-256','canonicalization_version':'RFC8785','domain':'psios/recovery-set-manifest/1'},
    'scope':'service_group','capture_interval':{'started_at':T,'completed_at':T},'consistency':'crash_consistent',
    'coordination':'IndependentCheckpointsWithReconciliation','consistency_evidence_refs':[],
    'members':[{'resource_id':'volume-demo','data_incarnation':'volume-incarnation-1','schema_version':'1.0.0',
                'checkpoint':'commit-42','backup_ref':rr('volume-backup'),'size_bytes':'1048576'}],
    'cross_service_relation_refs':[rr('relations-checkpoint')],'incremental_dependencies':[],
    'required_log_refs':[rr('wal-42')],'security_evidence_refs':[],'secret_refs':[rr('backup-key-reference')],
    'identity_mapping_ref':rr('restore-identity-map'),'effect_reconciliation_plan_ref':rr('effect-reconcile-plan'),
    'replay_prohibited_refs':[rr('operation-demo')],'retention_policy_ref':rr('retention-policy'),
    'failure_domain':'independent-backup-medium','restore_procedure_ref':rr('restore-procedure'),
    'independent_tool_ref':rr('restore-tool'),'required_restore_bytes':'2097152'},
 'application-manifest':{'schema_version':'1.0.0','application_id':'app-notes','release_id':'app-notes-release-1',
    'release_version':'0.1.0-prototype','artifact_ref':rr('oci-image-demo'),'publisher_identity_ref':rr('publisher-demo'),
    'runtime_profile_ref':rr('oci-profile'),'release_spec_ref':rr('notes-release-spec'),
    'package_format':'OCI','required_capability_refs':[rr('capability-volume-write')],
    'volume_requirements':[{'name':'data','minimum_bytes':'1073741824','access_mode':'single_writer_rw','schema_ref':rr('notes-data-schema')}],
    'network_policy_ref':rr('network-local-only'),'health_check_ref':rr('notes-health'),
    'migration_plan_ref':rr('notes-migrate'),'rollback_policy_ref':rr('notes-rollback'),
    'compatibility_claims':[{'capability':'Recognized','result':'NotRun','profile_ref':rr('oci-profile'),
                             'evidence_refs':[],'limitations':['Illustrative fixture only.']}]},
 'application-release-spec':{'schema_version':'1.0.0','release_spec_id':'notes-release-spec',
    'execution':{'argv':['/app/bin/notes','--data','/data'],'working_directory':'/app','environment':{'LANG':'C.UTF-8'}},
    'lifecycle':{'stop_signal':'SIGTERM','stop_grace_seconds':'20','start_deadline_seconds':'60',
                 'restart_policy':'OnFailure','max_restarts':'3'},
    'identity':{'execution_identity_profile_ref':rr('identity-unprivileged'),
                'namespace_mapping_profile_ref':rr('private-user-namespace')},
    'permission_requests':{'capability_refs':[rr('capability-volume-write')],'secret_refs':[],
        'device_profile_refs':[],'volume_bindings':[{'manifest_volume_name':'data','mount_path':'/data','requested_access':'ReadWrite'}]},
    'resources':{'cpu_millicores':'500','memory_max_bytes':'268435456','pids_max':'64',
                 'ephemeral_max_bytes':'67108864','io_profile_ref':rr('io-limited')},
    'provenance':{'source_refs':[rr('source-archive-demo')],'license_expression':'MIT',
        'license_material_ref':rr('license-demo'),'source_trust':'UserSuppliedUnverified',
        'trust_evidence_or_admission_policy_ref':rr('user-package-admission-policy'),
        'build_record_ref':rr('build-demo'),'sbom_ref':rr('sbom-demo')},
    'dependency_release_refs':[],'backup_scope_ref':rr('backup-notes-data')},
 'snapshot-page':{'schema_version':'1.0.0','authority_id':'storage-local','recovery_incarnation':'storage-incarnation-2',
    'snapshot_id':'snapshot-list-demo','items':[rr('volume-demo')],'next_cursor':None,'watch_start_cursor':'opaque-watch-demo',
    'expires_at':LATER,'authorization_epoch':'20','complete':True},
 'resource-envelope':{'api_version':'psios.v1','schema_version':'1.0.0','kind':'Volume',
    'metadata':{'resource_id':'volume-demo','identity_domain_id':'identity-origin','authority_id':'storage-local',
                'security_domain_id':'tenant-demo','owner_principal_id':'principal-user','resource_version':'rv-2',
                'spec_generation':'2','created_at':T,'updated_at':T},
    'spec':{'access_mode':'single_writer_rw'},'status':{'observed_generation':'2','lifecycle':'Available','conditions':[]}},
 'error':{'schema_version':'1.0.0','code':'OutcomeUnknown','message':'The external effect requires reconciliation.',
          'request_id':'request-demo','retryable':False,'retry_condition':'ReconcileOutcome','operation_id':'operation-demo'},
}

positives['recovery-set-manifest']=with_digest(positives['recovery-set-manifest'],'manifest_digest','recovery-set-manifest')
manifest=[]
for name, value in positives.items():
    p='positive/'+name+'.json'
    write(EXAMPLES/p,value)
    manifest.append({'id':'EX-P-'+name,'path':p,'schema':name+'.schema.json','expected':'Valid',
                     'validation':'SchemaAndLocalSemantics','purpose':schemas[name]['title']})

# Every invalid fixture carries an expected rejection category, so a failure from
# an unrelated or broken schema resolver cannot be counted as a useful pass.
def negative(name, schema, mutate, rejection, purpose):
    value=copy.deepcopy(positives[schema]); mutate(value)
    p='negative/'+name+'.json';write(EXAMPLES/p,value)
    manifest.append({'id':'EX-N-'+name,'path':p,'schema':schema+'.schema.json','expected':'Invalid',
                     'validation':rejection,'purpose':purpose})

negative('task-client-task-id','task-submit',lambda v:v.update(task_id='client-forged'),'Schema','Create request must not include server Task ID.')
negative('task-missing-origin-step','task-submit',lambda v:v['origin_ref'].pop('step_key'),'Schema','Workflow origins need stable step identity.')
negative('task-wrong-digest','task-submit',lambda v:v.update(task_type='unexpected.changed.v1'),'Digest','Digest covers all business fields.')
negative('chat-forged-sender','chat-send',lambda v:v.update(sender_principal_id='admin'),'Schema','Sender comes from authenticated identity.')
negative('chat-missing-client-id','chat-send',lambda v:v.pop('client_send_id'),'Schema','Client send ID is required.')
negative('chat-number-sequence','chat-accepted',lambda v:v.update(conversation_seq=9007199254740993),'Schema','Persistent sequence must not use JSON number.')
negative('chat-leading-zero-epoch','chat-accepted',lambda v:v.update(writer_epoch='012'),'Schema','Canonical decimal encoding has no leading zero.')
negative('chat-counter-overflow','chat-accepted',lambda v:v.update(writer_epoch='18446744073709551616'),'Semantic','u64 semantic upper bound.')
negative('operation-success-unknown','operation',lambda v:v.update(lifecycle='Succeeded'),'Schema','Unknown effects cannot be successful.')
negative('task-success-unknown','task',lambda v:v.update(outcome_knowledge='Unknown'),'Schema','Task success requires known effects.')
negative('run-success-unknown','run',lambda v:v.update(lifecycle='Succeeded'),'Schema','Run success requires known effects.')
negative('workflow-completed-alias','workflow-run',lambda v:v.update(lifecycle='Completed'),'Schema','Legacy Completed is not a v1 lifecycle.')
negative('approval-bound-no-operation','approval-use',lambda v:v.pop('operation_id'),'Schema','Bound usage must name its unique Operation.')
negative('approval-unbound-with-operation','approval-use',lambda v:v.update(binding_status='Unbound'),'Schema','Unbound usage must not retain a bound Operation.')
negative('permit-self-declared-proof','execution-permit',lambda v:v.update(auth_proof={'mode':'TrustMe'}),'Schema','Only explicit authenticated lookup or signature structures.')
negative('dataset-null-name','dataset-record',lambda v:v['record'].update(name=None),'Schema','Missing name differs from null.')
negative('dataset-duplicate-tags','dataset-record',lambda v:v['record'].update(tag=['book','book']),'Schema','Tags have set semantics.')
negative('dataset-empty-vector','dataset-record',lambda v:v['record'].update(vector=[]),'Schema','Absent vector is not an empty vector.')
negative('dataset-scored-without-profile','dataset-record',lambda v:v['record'].update(score=0),'Schema','Score zero is valid but must have a scoring profile.')
negative('readonly-attachment-writes','volume-attachment',lambda v:v.update(requested_access='ReadOnly'),'Schema','A read-only request cannot gain effective writes.')
negative('revocation-false-complete','revocation-result',lambda v:v.update(status='Complete'),'Semantic','Complete requires all required targets Enforced.')
negative('quarantine-auto-writes','recovery-status',lambda v:v.update(security_mode='Quarantined',automatic_writes_enabled=True),'Schema','Quarantine cannot enable automatic writes.')
negative('restore-unknown-delete-share','recovery-status',lambda v:v.update(public_sharing_enabled=True),'Schema','Unknown deletion history blocks automatic public sharing.')
negative('snapshot-false-final','snapshot-page',lambda v:v.update(next_cursor='more'),'Schema','Final page must have no next cursor.')
negative('workflow-duplicate-step','workflow-definition',lambda v:v['nodes'].append(copy.deepcopy(v['nodes'][0])),'Semantic','Steps need unique stable IDs.')
negative('workflow-cycle','workflow-definition',lambda v:v['nodes'][0].update(depends_on=['review']),'Semantic','Base graph must be acyclic.')
negative('workflow-zero-step-deadline','workflow-definition',lambda v:v['nodes'][0].update(deadline_seconds='0'),'Schema','All step deadlines must be finite positive bounds.')
def unbounded_loop(v):
    v['nodes']=[{'kind':'BoundedLoop','step_id':'loop','depends_on':[],'deadline_seconds':'600',
        'retry_policy':policy,'failure_policy':'Stop','body_definition_ref':rr('bounded-child-definition'),
        'while_expression':{'kind':'literal','value':True},'budget_ref':rr('budget-root')}]
negative('workflow-loop-missing-bound','workflow-definition',unbounded_loop,'Schema','A real BoundedLoop node must declare max_iterations.')
negative('task-success-pending-result','task',lambda v:v['result'].update(availability='Pending'),'Schema','Succeeded cannot retain an unpublished initial result.')
negative('dispatch-mismatched-task','dispatch-intent',lambda v:v['accepted'].update(request_digest=rr('different')['digest']),'Semantic','Linked receipt must match the exact dispatch intent.')
negative('runtime-verified-no-evidence','application-manifest',lambda v:v['compatibility_claims'][0].update(capability='RuntimeVerified',result='Passed'),'Schema','A Passed compatibility claim needs actual evidence references.')
negative('application-no-execution-spec','application-manifest',lambda v:v.pop('release_spec_ref'),'Schema','A release cannot omit its complete executable requirements.')
negative('application-unbounded-memory','application-release-spec',lambda v:v['resources'].update(memory_max_bytes='0'),'Schema','Memory hard limit must be a positive finite quantity.')
negative('lookup-found-no-record','task-lookup',lambda v:v.pop('accepted'),'Schema','Found needs the original acceptance receipt.')
negative('resource-observed-future','resource-envelope',lambda v:v['status'].update(observed_generation='3'),'Semantic','Observed generation cannot exceed desired generation.')

# Recompute checksums on semantic-negative Workflow definitions so graph failures
# are distinct from changed digests. Invalid structure fixtures need no digest pass.
for item in manifest:
    if item['expected']=='Invalid' and item['schema']=='workflow-definition.schema.json' and item['validation']=='Semantic':
        p=EXAMPLES/item['path'];v=json.loads(p.read_text());write(p,with_digest(v,'definition_digest','workflow-definition'))

# Additional positive fixtures exercise allowed absence/null/zero and big counters.
scored=copy.deepcopy(positives['dataset-record'])
scored['record'].update(score=0,vector=[1,0,0],extra_data=None)
scored['metadata'].update(score_profile_ref=rr('score-profile'),score_source_content_version='content-rev-1',
    embedding_profile={'profile_ref':rr('embedding-profile'),'model_id':'demo-model','model_version':'model-rev-1',
        'dimensions':'3','distance':'Cosine','normalization':'UnitL2','source_content_version':'content-rev-1'})
write(EXAMPLES/'positive/dataset-scored-vector.json',scored)
manifest.append({'id':'EX-P-dataset-scored-vector','path':'positive/dataset-scored-vector.json',
                 'schema':'dataset-record.schema.json','expected':'Valid','validation':'SchemaAndLocalSemantics',
                 'purpose':'score=0, optional vector with version-bound profile, and extra_data=null are valid.'})
for variant,status in [('expired','Expired'),('indeterminate','Indeterminate')]:
    v=copy.deepcopy(positives['task-lookup']);v.pop('accepted');v.update(status=status,reason='The authority cannot prove non-acceptance.')
    write(EXAMPLES/('positive/task-lookup-'+variant+'.json'),v)
    manifest.append({'id':'EX-P-task-lookup-'+variant,'path':'positive/task-lookup-'+variant+'.json',
                     'schema':'task-lookup.schema.json','expected':'Valid','validation':'SchemaAndLocalSemantics',
                     'purpose':'No automatic retry is inferred from this lookup status.'})

write(EXAMPLES/'manifest.json',{'schema_version':'1.0.0','scope':'DocumentationFixtureValidationOnly','examples':manifest})
vectors=[]
for name,field in [('task-submit','request_digest'),('chat-send','content_digest'),('operation-plan','plan_digest'),('workflow-definition','definition_digest'),('recovery-set-manifest','manifest_digest')]:
    payload=copy.deepcopy(positives[name]);expected=payload.pop(field)
    vectors.append({'contract':name,'digest_field':field,'domain_prefix':'psios/'+name+'/1\n',
                    'payload':payload,'canonical_utf8':restricted_jcs(payload).decode(),
                    'expected_digest':expected,'scope':'ASCII object keys and safe integer number subset; not complete RFC8785 conformance.'})
write(EXAMPLES/'digest-vectors.json',{'schema_version':'1.0.0','vectors':vectors})
for name, schema in schemas.items(): write(SCHEMA/(name+'.schema.json'),schema)
write(SCHEMA/'catalog.json',{'schema_version':'1.0.0','namespace':BASE,
    'binding':'Logical JSON contracts; HTTP/RPC transport and backend implementation are not supplied.',
    'contracts':[{'name':n,'path':n+'.schema.json','title':s['title']} for n,s in schemas.items()]})
print(json.dumps({'schemas':len(schemas),'positive_examples':sum(x['expected']=='Valid' for x in manifest),
                  'negative_examples':sum(x['expected']=='Invalid' for x in manifest),'digest_vectors':len(vectors)}))
