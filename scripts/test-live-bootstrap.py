#!/usr/bin/env python3
"""Focused offline tests: no provider calls, repository mutation or credential access."""
import argparse
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('bootstrap', Path(__file__).with_name('bootstrap-live-secrets.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def args(**kwargs):
    values = dict(repo='owner/repo', environment='production', only=['CLOUDFLARE_API_TOKEN'],
                  rotate=False, dry_run=False, publisher_key_file=None)
    values.update(kwargs)
    return argparse.Namespace(**values)


class BootstrapTests(unittest.TestCase):
    def runner(self, existing=(), protected=True):
        calls = []
        def run(argv, body=None):
            calls.append((argv, body))
            if argv[:3] == ['gh', 'secret', 'list']:
                return json.dumps([{'name': name} for name in existing])
            if 'deployment-branch-policies' in argv[-1]:
                return json.dumps({'branch_policies': [{'name': 'master', 'type': 'branch'}]})
            if argv[:2] == ['gh', 'api']:
                return json.dumps({'deployment_branch_policy': {'custom_branch_policies': True},
                                   'can_admins_bypass': False,
                                   'protection_rules': [{'type': 'required_reviewers',
                                     'reviewers': [1] if protected else [], 'prevent_self_review': True}]})
            return ''
        return run, calls

    def test_existing_skipped_without_reading_input(self):
        run, calls = self.runner(['CLOUDFLARE_API_TOKEN'])
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(args(), run, {})
        self.assertFalse(any(c[0][:3] == ['gh', 'secret', 'set'] for c in calls))

    def test_target_and_stdin(self):
        run, calls = self.runner()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            m.provision(args(), run, {'CLOUDFLARE_API_TOKEN': 'super-private-value'})
        write = calls[-1]
        self.assertEqual(write, (['gh', 'secret', 'set', 'CLOUDFLARE_API_TOKEN', '--repo',
                                 'owner/repo', '--env', 'production'], 'super-private-value'))
        self.assertNotIn('super-private-value', output.getvalue())
        self.assertFalse(any('PUT' in c[0] for c in calls))

    def test_never_weakens_protections(self):
        run, calls = self.runner(protected=False)
        with self.assertRaises(m.BootstrapError):
            m.provision(args(), run, {'CLOUDFLARE_API_TOKEN': 'private'})
        self.assertFalse(any(c[0][:3] == ['gh', 'secret', 'set'] for c in calls))

    def test_rotation_selected_only(self):
        run, calls = self.runner(['CLOUDFLARE_API_TOKEN'])
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(args(rotate=True), run, {'CLOUDFLARE_API_TOKEN': 'replacement'})
        self.assertEqual(len([c for c in calls if c[1]]), 1)
        self.assertFalse(any('DELETE' in c[0] for c in calls))

    def test_dry_run_no_api(self):
        def fail(*_):
            self.fail('dry run performed API request')
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(args(dry_run=True), fail, {})

    def test_key_private_digest_only(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'key'
            path.write_text('a' * 64)
            path.chmod(0o600)
            self.assertEqual(len(m.read_private_key(path)), 64)
            path.chmod(0o644)
            with self.assertRaises(m.BootstrapError):
                m.read_private_key(path)
            link = Path(directory) / 'link'
            link.symlink_to(path)
            with self.assertRaises(m.BootstrapError):
                m.read_private_key(link)

    def test_scopes_not_interchangeable(self):
        groups = [dict(name=name, id=name, scopes=[scope]) for name, scope in [
            ('Workers Scripts Write', 'com.cloudflare.api.account'),
            ('Pages Write', 'com.cloudflare.api.account'),
            ('Workers Routes Write', 'com.cloudflare.api.account.zone'),
            ('DNS Write', 'com.cloudflare.api.account.zone'),
            ('Zone Read', 'com.cloudflare.api.account.zone')]]
        policy = m.token_policy('account', 'zone', groups)['policies']
        self.assertEqual(list(policy[0]['resources']), ['com.cloudflare.api.account.account'])
        self.assertEqual(list(policy[1]['resources']), ['com.cloudflare.api.account.zone.zone'])
        groups[-1]['scopes'] = ['com.cloudflare.api.account']
        with self.assertRaises(m.BootstrapError):
            m.token_policy('account', 'zone', groups)

    def test_validate_all_before_write(self):
        run, calls = self.runner()
        with self.assertRaises(m.BootstrapError):
            m.provision(args(only=['CLOUDFLARE_API_TOKEN', 'LIVE_STATE_BUCKET']), run,
                        {'CLOUDFLARE_API_TOKEN': 'secret'})
        self.assertFalse(any(c[0][:3] == ['gh', 'secret', 'set'] for c in calls))

    def test_redacted_process_failure(self):
        from unittest.mock import patch
        import subprocess
        with patch.object(m, 'bounded_run', return_value=subprocess.CompletedProcess([], 1, 'secret', 'token')):
            with self.assertRaises(m.BootstrapError) as error:
                m.command(['gh', 'secret', 'set', 'name'], 'credential')
        self.assertNotIn('token', str(error.exception))
        self.assertNotIn('credential', str(error.exception))





def normal_args(**kwargs):
    values = dict(zone='bmux.dev', hostname=None, pages_project='bmux', input_env=False,
                  environment='Production', bootstrap_state=False, configure_oidc=False,
                  state_mode='plan', state_operation=None, state_region='us-east-1',
                  state_bucket='bmux-state', state_role='bmux-live', state_account_id='123456789012',
                  state_dir=Path('/private/state'), accept_cost=False, approve_import=False)
    values.update(kwargs)
    return args(**values)


def groups():
    return [{'name': name, 'id': str(index), 'scopes': [scope]}
            for index, (name, scope) in enumerate([
                ('Workers Scripts Write','com.cloudflare.api.account'),
                ('Pages Write','com.cloudflare.api.account'),
                ('Workers Routes Write','com.cloudflare.api.account.zone'),
                ('DNS Write','com.cloudflare.api.account.zone'),
                ('Zone Read','com.cloudflare.api.account.zone')])]


class FakeCloudflare:
    def __init__(self):
        self.calls = []
        self.zones = [{'id': 'b'*32, 'name':'bmux.dev', 'account': {'id':'a'*32}}]
        self.permissions = groups()
        self.projects = [{'name':'bmux','production_branch':'master'}]
    def items(self,path):
        self.calls.append(('GET',path,None))
        if path.startswith('/zones?'): return iter(self.zones)
        if path == '/user/tokens/permission_groups': return iter(self.permissions)
        if '/pages/projects' in path: return iter(self.projects)
        raise AssertionError(path)
    def request(self,method,path,body=None):
        self.calls.append((method,path,body))
        if method == 'POST' and path == '/user/tokens':
            return {'success':True,'result':{'id':'new','value':'BMUX-PRIVATE-TOKEN'}}
        raise AssertionError(path)


class TurnkeyBootstrapTests(unittest.TestCase):
    runner = BootstrapTests.runner
    def test_normal_existing_zero_provider(self):
        run,calls=self.runner(m.NORMAL_SECRET_NAMES)
        cf=FakeCloudflare()
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(normal_args(only=None),run,{},cf)
        self.assertEqual(cf.calls,[])
        self.assertFalse(any(c[0][:3]==['gh','secret','set'] for c in calls))

    def test_normal_token_is_bmux_specific_split_scope_stdin(self):
        run,calls=self.runner()
        cf=FakeCloudflare(); out=io.StringIO()
        with contextlib.redirect_stdout(out): m.provision(normal_args(),run,{},cf)
        create=[c for c in cf.calls if c[0]=='POST'][0]
        self.assertEqual(create[2]['name'],'bmux:github:owner/repo:Production:deployment')
        policies=create[2]['policies']
        self.assertEqual(list(policies[0]['resources']),['com.cloudflare.api.account.'+'a'*32])
        self.assertEqual(list(policies[1]['resources']),['com.cloudflare.api.account.zone.'+'b'*32])
        self.assertEqual(calls[-1][1],'BMUX-PRIVATE-TOKEN')
        self.assertNotIn('BMUX-PRIVATE-TOKEN',out.getvalue())
        self.assertFalse(any(c[0]=='DELETE' for c in cf.calls))

    def test_partial_existing_ids_not_rewritten(self):
        run,calls=self.runner(['CLOUDFLARE_ACCOUNT_ID','CLOUDFLARE_ZONE_ID'])
        cf=FakeCloudflare()
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(normal_args(only=['CLOUDFLARE_ACCOUNT_ID','CLOUDFLARE_ZONE_ID','LIVE_HOSTNAME']),run,{},cf)
        self.assertEqual(cf.calls,[])
        writes=[c for c in calls if c[0][:3]==['gh','secret','set']]
        self.assertEqual([c[0][3] for c in writes],['LIVE_HOSTNAME'])
        self.assertEqual(writes[0][1],'bmux.dev')

    def test_zone_ambiguity_no_mutations(self):
        run,calls=self.runner(); cf=FakeCloudflare(); cf.zones *= 2
        with self.assertRaises(m.BootstrapError): m.provision(normal_args(),run,{},cf)
        self.assertFalse(any(c[0]=='POST' for c in cf.calls))
        self.assertFalse(any(c[0][:3]==['gh','secret','set'] for c in calls))

    def test_existing_pages_exact_master_and_direct_upload(self):
        run,calls=self.runner(); cf=FakeCloudflare(); cf.projects[0]['source']={'type':'github'}
        with self.assertRaises(m.BootstrapError):
            m.provision(normal_args(only=['LIVE_PAGES_PROJECT']),run,{},cf)
        self.assertFalse(any(c[0][:3]==['gh','secret','set'] for c in calls))

    def test_private_generation_repeat_no_plaintext(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'key'; out=io.StringIO()
            run,calls=self.runner()
            with contextlib.redirect_stdout(out):
                m.provision(normal_args(only=['LIVE_PUBLISHER_KEY_SHA256'],publisher_key_file=path),run,{})
            raw=path.read_bytes()
            self.assertEqual(path.stat().st_mode & 0o777,0o600)
            self.assertNotIn(raw.decode(),out.getvalue())
            self.assertNotIn(raw.decode(),str(calls))
            self.assertEqual(m.private_key_digest(path),calls[-1][1])
            self.assertEqual(path.read_bytes(),raw)

    def test_key_symlink_never_replaced(self):
        with tempfile.TemporaryDirectory() as directory:
            target=Path(directory)/'target'; target.write_text('x'*48); target.chmod(0o600)
            link=Path(directory)/'key'; link.symlink_to(target)
            with self.assertRaises(m.BootstrapError):m.private_key_digest(link)
            self.assertEqual(target.read_text(),'x'*48)

    def test_self_review_allowed_separate_gate(self):
        run,calls=self.runner()
        def runner(argv,body=None):
            value=run(argv,body)
            if argv[:2]==['gh','api'] and 'deployment-branch-policies' not in argv[-1]:
                data=json.loads(value); data['protection_rules'][0]['prevent_self_review']=False
                return json.dumps(data)
            return value
        with contextlib.redirect_stdout(io.StringIO()):m.provision(normal_args(),runner,{},FakeCloudflare())
        self.assertTrue(any('production-approval' in c[0][-1] for c in calls))
        self.assertFalse(any('PUT' in c[0] for c in calls))

    def test_ambiguous_write_never_revokes_or_retries(self):
        run,calls=self.runner(); cf=FakeCloudflare()
        def runner(argv,body=None):
            if argv[:3]==['gh','secret','set']:raise m.BootstrapError('ambiguous write')
            return run(argv,body)
        with self.assertRaises(m.BootstrapError): m.provision(normal_args(),runner,{},cf)
        self.assertEqual(len([c for c in cf.calls if c[0]=='POST']),1)
        self.assertFalse(any(c[0]=='DELETE' for c in cf.calls))

    def test_rotation_requires_selection_before_provider(self):
        def fail(*_):self.fail('access before rotate validation')
        with self.assertRaises(m.BootstrapError):m.provision(normal_args(rotate=True,only=None),fail,{})

    def test_dryrun_zero_calls_and_no_key_creation(self):
        def fail(*_):self.fail('dry run accessed provider')
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'key'
            with contextlib.redirect_stdout(io.StringIO()):
                m.provision(normal_args(dry_run=True,bootstrap_state=True,publisher_key_file=path),fail,{})
            self.assertFalse(path.exists())

    def test_permission_pagination_and_duplicate_rejection(self):
        cf=object.__new__(m.Cloudflare); calls=[]
        def request(method,path,body=None):
            calls.append(path)
            page=2 if 'page=2&' in path else 1
            return {'result': groups()[2:] if page==2 else groups()[:2], 'result_info':{'total_pages':2}}
        cf.request=request
        permissions=list(cf.items('/user/tokens/permission_groups'))
        m.token_policy('account','zone',permissions)
        self.assertEqual(len(calls),2)
        with self.assertRaises(m.BootstrapError): m.token_policy('account','zone',permissions+[permissions[0]])

    def test_provider_error_sanitized(self):
        from unittest.mock import patch
        import urllib.error
        cf=m.Cloudflare({'CLOUDFLARE_PROVISION_TOKEN':'secret'})
        with patch.object(m.urllib.request,'build_opener') as opener:
            opener.return_value.open.side_effect=urllib.error.URLError('secret raw content')
            with self.assertRaises(m.BootstrapError) as error:cf.request('POST','/user/tokens',{})
        self.assertNotIn('secret raw content',str(error.exception))

    def test_oidc_custom_different_never_put(self):
        calls=[]
        def runner(argv,body=None):calls.append(argv);return json.dumps({'use_default':False,'include_claim_keys':['repo']})
        with self.assertRaises(m.BootstrapError):m.verify_oidc('owner/repo',runner,True)
        self.assertFalse(any('PUT' in c for c in calls))

    def test_oidc_default_requires_explicit_flag_and_verifies(self):
        calls=[]
        def runner(argv,body=None):
            calls.append((argv,body))
            return json.dumps({'use_default':len(calls)==1,'include_claim_keys':m.OIDC_CLAIMS})
        with self.assertRaises(m.BootstrapError):m.verify_oidc('owner/repo',runner,False)
        calls.clear()
        with contextlib.redirect_stdout(io.StringIO()):m.verify_oidc('owner/repo',runner,True)
        self.assertEqual(json.loads(calls[1][1]),{'use_default':False,'include_claim_keys':m.OIDC_CLAIMS})

    def test_later_branch_policy_page_blocks(self):
        calls=[]
        def runner(argv,body=None):
            calls.append(argv)
            if 'deployment-branch-policies' in argv[-1]:
                row={'name':'other' if 'page=2' in argv[-1] else 'master','type':'branch'}
                return json.dumps({'branch_policies':[row],'total_count':2})
            return json.dumps({'deployment_branch_policy':{'custom_branch_policies':True}})
        with self.assertRaises(m.BootstrapError):m.verify_environment('owner/repo','Production',runner)
        self.assertEqual(len(calls),3)

    def test_backend_metadata_installed_without_static_keys(self):
        run,calls=self.runner(); child=[]
        result={'schema':1,'operation':'operation123','phase':'complete',
                'backend':{'bucket':'bmux-state','region':'us-east-1','key':'bmux/live/production/terraform.tfstate','encrypt':True,'use_lockfile':True},
                'role_arn':'arn:aws:iam::123456789012:role/bmux-live',
                'github_oidc':{'include_claim_keys':m.OIDC_CLAIMS,'subject':'repo:owner/repo:environment:Production:ref:refs/heads/master'}}
        def runner(argv,body=None):
            if argv[0]==m.sys.executable:
                child.append(argv)
                if '--review' in argv:self.assertEqual(argv[-1],'--review')
                data=dict(result)
                if not any(flag in argv for flag in ['--apply','--review']):data.update(operation=None,phase='dryrun')
                return json.dumps(data)
            if '/actions/oidc/' in argv[-1]:return json.dumps({'use_default':False,'include_claim_keys':m.OIDC_CLAIMS})
            return run(argv,body)
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(normal_args(only=list(m.STATE_SECRET_NAMES),bootstrap_state=True,state_mode='apply',state_operation='operation123',accept_cost=True),runner,{})
        self.assertIn('--review',child[1]);self.assertIn('--apply',child[2])
        self.assertIn('--accept-cost',child[2])
        writes=[c for c in calls if c[0][:3]==['gh','secret','set']]
        self.assertEqual([c[0][3] for c in writes],list(m.STATE_SECRET_NAMES))
        self.assertFalse(any('ACCESS_KEY' in c[0][3] for c in writes))

    def backend_fixture(self, phase='planned', operation='operation123'):
        return dict(schema=1,operation=operation,phase=phase,
                    backend=dict(bucket='bmux-state',region='us-east-1',key='bmux/live/production/terraform.tfstate',encrypt=True,use_lockfile=True),
                    role_arn='arn:aws:iam::123456789012:role/bmux-live',
                    github_oidc=dict(include_claim_keys=m.OIDC_CLAIMS,subject='repo:owner/repo:environment:Production:ref:refs/heads/master'),
                    resources=[dict(address='aws_s3_bucket.state["production"]',actions=['create'],**{'import':False})])

    def test_backend_plan_never_creates_partial_credentials(self):
        run,calls=self.runner();cf=FakeCloudflare();child=[]
        def runner(argv,body=None):
            if argv[0]==m.sys.executable:
                child.append(argv); result=self.backend_fixture('planned' if '--plan' in argv else 'dryrun','operation123' if '--plan' in argv else None)
                result['resources']=[]
                return json.dumps(result)
            if '/actions/oidc/' in argv[-1]:return json.dumps(dict(use_default=False,include_claim_keys=m.OIDC_CLAIMS))
            return run(argv,body)
        with tempfile.TemporaryDirectory() as directory, contextlib.redirect_stdout(io.StringIO()):
            path=Path(directory)/'key'
            m.provision(normal_args(only=None,bootstrap_state=True,publisher_key_file=path),runner,{},cf)
            self.assertFalse(path.exists())
        self.assertEqual(cf.calls,[])
        self.assertFalse(any(c[0][:3]==['gh','secret','set'] for c in calls))

    def test_guided_cancel_no_token_or_secret_writes(self):
        from unittest.mock import patch
        run,calls=self.runner();cf=FakeCloudflare()
        def runner(argv,body=None):
            if argv[0]==m.sys.executable:
                result=self.backend_fixture('dryrun' if '--plan' not in argv and '--review' not in argv else 'planned',None if '--plan' not in argv and '--review' not in argv else 'operation123')
                result['resources']=[{'address':'aws_s3_bucket.state["production"]','actions':['create'],'import':False}]
                self.assertNotIn('--apply',argv)
                return json.dumps(result)
            if '/actions/oidc/' in argv[-1]:return json.dumps(dict(use_default=False,include_claim_keys=m.OIDC_CLAIMS))
            return run(argv,body)
        with patch.object(m.sys,'stdin') as stdin,patch.object(m.sys,'stderr') as stderr,contextlib.redirect_stdout(io.StringIO()):
            stdin.isatty.return_value=True;stderr.isatty.return_value=True;stdin.readline.return_value='cancel\n'
            with self.assertRaises(m.BootstrapError):m.provision(normal_args(only=None,bootstrap_state=True,state_mode='guided',publisher_key_file=Path('/private/key')),runner,{},cf)
        self.assertEqual(cf.calls,[])
        self.assertFalse(any(c[0][:3]==['gh','secret','set'] for c in calls))

    def test_explicit_backend_runs_even_all_secrets_existing(self):
        run,calls=self.runner(m.NORMAL_SECRET_NAMES+m.STATE_SECRET_NAMES);child=[]
        def runner(argv,body=None):
            if argv[0]==m.sys.executable:
                child.append(argv);result=self.backend_fixture('planned' if '--plan' in argv else 'dryrun','operation123' if '--plan' in argv else None);result['resources']=[];return json.dumps(result)
            if '/actions/oidc/' in argv[-1]:return json.dumps(dict(use_default=False,include_claim_keys=m.OIDC_CLAIMS))
            return run(argv,body)
        with contextlib.redirect_stdout(io.StringIO()):m.provision(normal_args(only=None,bootstrap_state=True),runner,{})
        self.assertTrue(any('--plan' in c for c in child))

    def test_backend_mismatched_result_blocks_install(self):
        result=self.backend_fixture('complete');result['resources']=[];result['role_arn']='arn:aws:iam::123456789012:role/other'
        with self.assertRaises(m.BootstrapError):m.validate_state_result(result,normal_args(),'operation123',('complete',))

    def test_existing_bucket_conflict_blocks_before_oidc(self):
        def fail(*_):self.fail('conflict accessed provider')
        with self.assertRaises(m.BootstrapError):m.state_bootstrap(normal_args(bootstrap_state=True),fail,['LIVE_STATE_BUCKET'],{'LIVE_STATE_BUCKET':'other'})

    def test_complete_normal_default_bootstrap(self):
        run,calls=self.runner();cf=FakeCloudflare()
        with tempfile.TemporaryDirectory() as directory,contextlib.redirect_stdout(io.StringIO()):
            m.provision(normal_args(only=None,publisher_key_file=Path(directory)/'key'),run,{},cf)
        writes=[c for c in calls if c[0][:3]==['gh','secret','set']]
        self.assertEqual({c[0][3] for c in writes},set(m.NORMAL_SECRET_NAMES))
        self.assertTrue(all(c[0][-1]=='Production' for c in writes))
        self.assertEqual(len([c for c in cf.calls if c[0]=='POST']),1)

    def test_normal_rotation_only_selected(self):
        run,calls=self.runner(m.NORMAL_SECRET_NAMES);cf=FakeCloudflare()
        with contextlib.redirect_stdout(io.StringIO()):m.provision(normal_args(rotate=True),run,{},cf)
        writes=[c for c in calls if c[0][:3]==['gh','secret','set']]
        self.assertEqual([c[0][3] for c in writes],['CLOUDFLARE_API_TOKEN'])
        self.assertFalse(any(c[0]=='DELETE' for c in cf.calls))

    def test_destructive_or_private_review_rejected(self):
        result=self.backend_fixture();result['resources'][0].pop('import_',None)
        result['resources'][0]['actions']=['delete'];result['resources'][0]['import']=False
        with self.assertRaises(m.BootstrapError):m.validate_state_result(result,normal_args(),'operation123',('planned',))
        result=self.backend_fixture();result['credential']='hidden'
        with self.assertRaises(m.BootstrapError):m.validate_state_result(result,normal_args(),'operation123',('planned',))

    def test_legacy_approval_allows_self_review(self):
        run,calls=self.runner()
        def runner(argv,body=None):
            text=run(argv,body)
            if argv[:2]==['gh','api'] and 'deployment-branch-policies' not in argv[-1]:
                value=json.loads(text);value['protection_rules'][0]['prevent_self_review']=False;return json.dumps(value)
            return text
        with contextlib.redirect_stdout(io.StringIO()):m.provision(args(),runner,{'CLOUDFLARE_API_TOKEN':'private'})

    def test_real_companion_review_parser_contract_offline(self):
        from unittest.mock import patch
        companion_path=Path(__file__).with_name('provision-live-state.py')
        spec=importlib.util.spec_from_file_location('backend_contract',companion_path)
        backend=importlib.util.module_from_spec(spec);spec.loader.exec_module(backend)
        reviewed=self.backend_fixture();reviewed['resources']=[]
        run,calls=self.runner();child=[]
        def runner(argv,body=None):
            if argv[0]==m.sys.executable:
                child.append(argv)
                if '--review' not in argv:
                    value=self.backend_fixture('dryrun',None);value['resources']=[];return json.dumps(value)
                # Exercise actual argparse, while replacing all IO/provider owners.
                with patch.object(backend,'Tools'),patch.object(backend,'Provision') as provision,patch.object(backend.local,'lock') as lock,patch.object(backend.os,'umask'),contextlib.redirect_stdout(io.StringIO()) as output:
                    lock.return_value.__enter__.return_value=None
                    provision.return_value.review.return_value=reviewed
                    self.assertEqual(backend.main(argv[2:]),0)
                return output.getvalue()
            return run(argv,body)
        with contextlib.redirect_stdout(io.StringIO()):
            m.provision(normal_args(only=list(m.STATE_SECRET_NAMES),bootstrap_state=True,state_mode='review',state_operation='operation123'),runner,{})
        self.assertEqual(child[-1][-1],'--review')
        self.assertFalse(any(c[0][:3]==['gh','secret','set'] for c in calls))

    def test_companion_deadline_only_exact_selected_script(self):
        from unittest.mock import patch
        import subprocess
        companion=str(Path(m.__file__).with_name('provision-live-state.py'))
        with patch.object(m,'bounded_run',return_value=subprocess.CompletedProcess([],0,'{}','')) as run:
            m.command([m.sys.executable,companion,'--plan'])
            self.assertEqual(run.call_args.kwargs['timeout'],1000)
            m.command(['gh','api','repos/owner/repo'])
            self.assertEqual(run.call_args.kwargs['timeout'],120)
            m.command([m.sys.executable,'/other/provision-live-state.py'])
            self.assertEqual(run.call_args.kwargs['timeout'],120)

    def test_real_companion_timeout_reaps_child_and_fences_retry(self):
        from unittest.mock import patch
        import subprocess
        companion=str(Path(m.__file__).with_name('provision-live-state.py'))
        actual_run=m.bounded_run
        started=[]
        actual_popen=subprocess.Popen
        def capture(*argv,**kwargs):
            child=actual_popen(*argv,**kwargs);started.append(child);return child
        def offline_run(argv,body=None,**kwargs):
            # Keep production timeout classification; substitute an inert local process.
            return actual_run([m.sys.executable,'-c','import time; time.sleep(10)'],body,**kwargs)
        with patch.object(m,'STATE_COMMAND_TIMEOUT',0.05),patch.object(m,'bounded_run',side_effect=offline_run),patch.object(m.subprocess,'Popen',side_effect=capture):
            with self.assertRaises(m.BootstrapError) as error:
                m.command([m.sys.executable,companion,'--apply','operation123','--accept-cost'])
        self.assertEqual(len(started),1)
        self.assertIsNotNone(started[0].poll())
        self.assertIn('operation ambiguous',str(error.exception))
        self.assertIn('reconcile exact operation ID',str(error.exception))
        self.assertNotIn('time.sleep',str(error.exception))

    def test_streaming_output_cap_kills_owned_process_and_redacts(self):
        from unittest.mock import patch
        actual_popen=m.subprocess.Popen; children=[]
        def capture(*argv,**kwargs):
            child=actual_popen(*argv,**kwargs);children.append(child);return child
        for channel in ('stdout','stderr'):
            script='import os,time; os.write(%s,b"private-output"*10000); time.sleep(10)' % (1 if channel=='stdout' else 2)
            with patch.object(m,'COMMAND_OUTPUT_LIMIT',1024),patch.object(m.subprocess,'Popen',side_effect=capture):
                with self.assertRaises(m.BootstrapError) as error:
                    m.command([m.sys.executable,'-c',script])
            self.assertIsNotNone(children[-1].poll())
            self.assertIn('operation ambiguous',str(error.exception))
            self.assertNotIn('private-output',str(error.exception))

    def test_bounded_stdin_and_output_success(self):
        result=m.bounded_run([m.sys.executable,'-c','import sys; value=sys.stdin.read(); sys.stderr.write("hidden"); sys.stdout.write(value)'],'private-stdin',timeout=2)
        self.assertEqual(result.stdout,'private-stdin')
        self.assertEqual(result.stderr,'')
        self.assertEqual(result.returncode,0)


if __name__ == '__main__':
    unittest.main()
