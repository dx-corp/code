import copy
import json
from pathlib import Path
import tempfile
import unittest

from billing import TOKENS, export_query, non_execution, priced_tokens, reconcile, summarize


class BillingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name)
        self.receipt = dict(type="managed_gateway_receipt", request_id="r", record_id="record", lineage_id="lineage")
        (self.path/"events.jsonl").write_text(json.dumps(self.receipt)+"\n")
        self.rates = dict(provider="fireworks", model="model", usd_per_million=dict(
            input_tokens="0.15", cache_read_tokens="0.03", cache_write_tokens=None, output_tokens="0.5"))
        self.row = dict(case="a", arm="fast", success=True, elapsed_seconds=1,
                        selected_path=str(self.path), tokens_complete=True,
                        input_tokens=100, cache_read_tokens=1000, cache_write_tokens=0, output_tokens=10)
        self.record = dict(organization_id="org", workspace_id="ws", request_id="r", record_id="record",
                           lineage_id="lineage", provider="fireworks", model="model", lifecycle_state="succeeded",
                           attempts=[dict(ordinal=0, outcome="succeeded", retry_max_attempts=3)])
        metadata = {k:self.record[k] for k in ("organization_id","workspace_id","record_id","lineage_id")}
        metadata.update(pricing_available=False, pricing_source="unconfigured", pricing_version="")
        self.record['usage_delivery'] = dict(body=dict(request_id="r",provider="fireworks",model="model",
            metadata=metadata,cost_micros=0, data={f:self.row[f] for f in TOKENS},
            **{f:self.row[f] for f in TOKENS}))

    def run_reconcile(self, records=None):
        return reconcile([self.row], records if records is not None else [self.record], 'org','ws',self.rates)[0]

    def test_placeholder_zero_never_becomes_free_usage(self):
        result = self.run_reconcile()
        self.assertEqual(result['list_price_estimate_usd'], '0.000050')
        self.assertIsNone(result['gateway_recorded_cost_usd'])
        self.assertIsNone(result['billed_cost_usd'])

    def test_missing_and_duplicate_records(self):
        self.assertIsNone(self.run_reconcile([])['list_price_estimate_usd'])
        with self.assertRaisesRegex(ValueError,'duplicate'):
            self.run_reconcile([self.record, self.record])

    def test_tenant_and_receipt_mismatch(self):
        self.record['workspace_id']='other'
        with self.assertRaisesRegex(ValueError,'tenant'):
            self.run_reconcile()
        self.record['workspace_id']='ws'
        self.record['record_id']='other'
        with self.assertRaisesRegex(ValueError,'identity'):
            self.run_reconcile()

    def test_auxiliary_usage_is_included_once(self):
        extra = copy.deepcopy(self.record)
        extra.update(request_id='aux',record_id='aux-record')
        extra['usage_delivery']['body']['request_id']='aux'
        extra['usage_delivery']['body']['metadata']['record_id']='aux-record'
        result=self.run_reconcile([self.record,extra])
        self.assertEqual(result['list_price_estimate_usd'],'0.000100')
        self.assertEqual(result['extra_request_ids'],['aux'])

    def test_failed_task_spend_stays_in_numerator(self):
        first=self.run_reconcile()
        second=dict(first,case='b',success=False)
        summary=summarize([first,second],False)
        self.assertEqual(summary['arms']['fast']['list_price_per_success_usd'],'0.000100')
        self.assertFalse(summary['intervals']['available'])

    def test_unknown_cache_write_price_is_not_zero(self):
        self.record['usage_delivery']['body']['cache_write_tokens']=1
        self.assertIsNone(self.run_reconcile()['list_price_estimate_usd'])

    def test_failed_provider_attempt_cannot_claim_complete_spend(self):
        self.record['attempts'].append(dict(ordinal=1,outcome='failed'))
        self.assertIsNone(self.run_reconcile()['list_price_estimate_usd'])

    def test_invalid_rates_and_tokens(self):
        for value in ('NaN', '-1'):
            self.rates['usd_per_million']['input_tokens']=value
            with self.assertRaisesRegex(ValueError,'price'):
                self.run_reconcile()
        self.rates['usd_per_million']['input_tokens']='0.15'
        self.record['usage_delivery']['body']['input_tokens']=True
        self.assertIsNone(self.run_reconcile()['list_price_estimate_usd'])

    def test_query_is_read_only_scoped_and_escapes_identifiers(self):
        sql=export_query([self.row],"o'rg",'ws')
        self.assertTrue(sql.startswith('BEGIN READ ONLY;'))
        self.assertIn("r.organization_id='o''rg'",sql)
        self.assertIn("r.workspace_id='ws'",sql)
        self.assertIn("r.lineage_id IN ('lineage')",sql)
        self.assertTrue(sql.endswith('ROLLBACK;\n'))

    def test_gateway_flattened_missing_usage_is_not_zero(self):
        body = self.record['usage_delivery']['body']
        body['input_tokens'] = 0
        body['data']['input_tokens'] = None
        self.assertIsNone(self.run_reconcile()['list_price_estimate_usd'])

    def test_absent_fireworks_cache_write_counter_matches_gateway_zero(self):
        self.record['usage_delivery']['body']['data']['cache_write_tokens'] = None
        self.assertEqual(self.run_reconcile()['list_price_estimate_usd'], '0.000050')

    def test_auxiliary_cannot_hide_native_usage_mismatch(self):
        extra = copy.deepcopy(self.record)
        extra.update(request_id='aux',record_id='aux-record')
        extra['usage_delivery']['body']['request_id']='aux'
        extra['usage_delivery']['body']['metadata']['record_id']='aux-record'
        body = self.record['usage_delivery']['body']
        body['input_tokens']=1
        body['data']['input_tokens']=1
        result=self.run_reconcile([self.record, extra])
        self.assertIn('gateway_native_token_mismatch',result['reasons'])
        self.assertIsNone(result['list_price_estimate_usd'])

    def test_missing_usage_does_not_hide_provider_retries(self):
        self.record['usage_delivery']=None
        self.record['attempts']=[dict(ordinal=0,outcome='failed'),dict(ordinal=1,outcome='succeeded')]
        result=self.run_reconcile()
        self.assertEqual(result['gateway_retries_observed'],1)
        self.assertIsNone(result['list_price_estimate_usd'])

    def test_explicit_versioned_zero_is_recorded_but_not_invoice_proof(self):
        metadata=self.record['usage_delivery']['body']['metadata']
        metadata.update(pricing_available=True,pricing_source='provider_ref',pricing_version='zero-v1')
        result=self.run_reconcile()
        self.assertEqual(result['gateway_recorded_cost_usd'],'0')
        self.assertIsNone(result['billed_cost_usd'])

    def test_partial_spend_stays_unknown_but_has_explicit_lower_bound(self):
        extra=copy.deepcopy(self.record)
        extra.update(request_id='missing',record_id='missing-record',usage_delivery=None)
        result=self.run_reconcile([self.record,extra])
        self.assertIsNone(result['list_price_estimate_usd'])
        self.assertEqual(result['list_price_lower_bound_usd'],'0.000050')
        candidate=dict(self.run_reconcile(),arm='minimal')
        result['success']=False
        second=dict(self.run_reconcile(),case='b')
        second_candidate=dict(candidate,case='b')
        summary=summarize([result,candidate,second,second_candidate],True)
        self.assertIsNone(summary['arms']['fast']['list_price_estimate_usd'])
        self.assertEqual(summary['candidate_baseline_list_price_ratio_upper_bound'],'0.5')
        self.assertNotIn('list_price_per_success_ratio_95',summary['intervals'])

    def test_preexecution_failure_requires_ledger_and_complete_timeline(self):
        record=copy.deepcopy(self.record)
        record.update(attempts=None,lifecycle_state='failed',terminal_http_status=503,error_code='http_error',usage_delivery=None)
        timeline={k:record[k] for k in ('request_id','organization_id','workspace_id')}
        stages=['decode','auth','plan','admission','prompts','token_estimate','rate_limit','token_budget','governance_input']
        phases=[dict(stage=s,outcome='accepted') for s in stages]
        phases[-1]['outcome']='rejected'
        timeline.update(terminal_status=503,error_code='http_error',phases=json.dumps(phases))
        self.assertTrue(non_execution(record,timeline))
        self.assertFalse(non_execution(record,None))
        record['attempts']=[dict(ordinal=0,outcome='failed')]
        self.assertFalse(non_execution(record,timeline))
        record['attempts']=None
        timeline['phases']=json.dumps(phases[1:])
        self.assertFalse(non_execution(record,timeline))
        timeline['phases']=json.dumps(phases+[dict(stage='provider',outcome='accepted')])
        self.assertFalse(non_execution(record,timeline))

    def test_preexecution_failure_does_not_hide_later_usage(self):
        extra=copy.deepcopy(self.record)
        extra.update(request_id='pre',record_id='pre-record',attempts=None,lifecycle_state='failed',terminal_http_status=503,error_code='http_error',usage_delivery=None)
        stages=['decode','auth','plan','admission','prompts','token_estimate','rate_limit','token_budget','governance_input']
        phases=[dict(stage=s,outcome='accepted') for s in stages]
        phases[-1]['outcome']='rejected'
        timeline={k:extra[k] for k in ('request_id','organization_id','workspace_id')}
        timeline.update(terminal_status=503,error_code='http_error',phases=json.dumps(phases))
        result=reconcile([self.row],[self.record,extra],'org','ws',self.rates,{'pre':timeline})[0]
        self.assertEqual(result['list_price_estimate_usd'],'0.000050')
        self.assertEqual(result['verified_non_execution_requests'],1)
