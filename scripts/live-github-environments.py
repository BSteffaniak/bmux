"""Private GitHub environment adapter for the live bootstrap plan/review/apply flow."""
import copy
import importlib.util
import json
from pathlib import Path
import re
import urllib.parse

spec = importlib.util.spec_from_file_location('live_local', Path(__file__).with_name('live-install.py'))
local = importlib.util.module_from_spec(spec)
spec.loader.exec_module(local)
Error = local.InstallError


def api(run, endpoint, method='GET', body=None):
    args = ['gh', 'api', endpoint]
    if method != 'GET':
        args += ['--method', method]
    if body is not None:
        args += ['--input', '-']
    raw = run(args, json.dumps(body) if body is not None else None)
    return json.loads(raw) if raw.strip() else None


def pages(run, endpoint, field):
    rows = []
    for page in range(1, 31):
        result = api(run, endpoint + f'?per_page=100&page={page}')
        if (not isinstance(result, dict) or not isinstance(result.get(field), list)
                or type(result.get('total_count')) is not int or not 0 <= result['total_count'] <= 3000):
            raise Error('Unsupported GitHub inventory; no mutation performed')
        batch = result[field]
        rows.extend(batch)
        if len(rows) == result['total_count']:
            return rows
        if not batch or len(rows) > result['total_count']:
            break
    raise Error('Incomplete GitHub inventory; no mutation performed')


def positive_id(value):
    if type(value) is not int or value <= 0:
        raise Error('Invalid GitHub resource identity')
    return value


class Environments:
    def __init__(self, args, run):
        self.args, self.run = args, run
        self.root = args.state_dir / 'github-environments'
        self.names = (args.environment, 'production-approval')
        if args.environment == 'production-approval':
            raise Error('Deployment and approval environments must be distinct')
        login = args.approval_reviewer
        if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9-]{0,38}', login):
            raise Error('--approval-reviewer must be a GitHub user login')
        user = api(run, 'users/' + login)
        if (not isinstance(user, dict) or user.get('type') != 'User'
                or str(user.get('login', '')).lower() != login.lower()):
            raise Error('Approval reviewer must resolve to the selected GitHub user')
        self.reviewer = {'type': 'User', 'id': positive_id(user.get('id'))}
        self.binding = dict(repo=args.repo, environment=args.environment, reviewer=self.reviewer,
                            implementation={p: local.digest(Path(__file__).with_name(p)) for p in
                                            ('live-github-environments.py', 'bootstrap-live-secrets.py', 'live-install.py')})
        self.current = self.snapshot()

    def endpoint(self, name):
        return f'repos/{self.args.repo}/environments/' + urllib.parse.quote(name, safe='')

    def snapshot(self):
        inventory = pages(self.run, f'repos/{self.args.repo}/environments', 'environments')
        if any(not isinstance(v, dict) or not isinstance(v.get('name'), str) for v in inventory):
            raise Error('Invalid GitHub environment inventory')
        names = [v['name'] for v in inventory]
        if len(names) != len(set(n.lower() for n in names)):
            raise Error('Duplicate GitHub environments')
        if any(n.lower() in {v.lower() for v in self.names} and n not in self.names for n in names):
            raise Error('GitHub environment capitalization differs; reconcile explicit names')
        result = {}
        for name in self.names:
            if name not in names:
                result[name] = None
                continue
            data = api(self.run, self.endpoint(name))
            if (not isinstance(data, dict) or data.get('name') != name
                    or type(data.get('can_admins_bypass')) is not bool
                    or not isinstance(data.get('protection_rules'), list)):
                raise Error('Unsupported GitHub environment configuration')
            config = dict(wait_timer=0, reviewers=[], prevent_self_review=False,
                          can_admins_bypass=data['can_admins_bypass'],
                          deployment_branch_policy=data.get('deployment_branch_policy'))
            policy = config['deployment_branch_policy']
            if policy is not None and (not isinstance(policy, dict) or set(policy) != {'protected_branches', 'custom_branch_policies'}
                                       or any(type(v) is not bool for v in policy.values())):
                raise Error('Unsupported GitHub branch policy')
            seen = set()
            for rule in data['protection_rules']:
                kind = rule.get('type')
                if kind in seen:
                    raise Error('Duplicate GitHub protection rule')
                seen.add(kind)
                if kind == 'wait_timer':
                    timer = rule.get('wait_timer')
                    if type(timer) is not int or not 0 <= timer <= 43200:
                        raise Error('Invalid GitHub wait timer')
                    config['wait_timer'] = timer
                elif kind == 'required_reviewers':
                    reviewers = rule.get('reviewers')
                    if type(rule.get('prevent_self_review')) is not bool or not isinstance(reviewers, list):
                        raise Error('Invalid GitHub reviewer configuration')
                    config['prevent_self_review'] = rule['prevent_self_review']
                    for reviewer in reviewers:
                        if reviewer.get('type') not in ('User', 'Team'):
                            raise Error('Unknown GitHub reviewer type')
                        config['reviewers'].append(dict(type=reviewer['type'], id=positive_id(reviewer['reviewer'].get('id'))))
                    config['reviewers'].sort(key=lambda v: (v['type'], v['id']))
                elif kind == 'branch_policy':
                    pass  # Represented by deployment_branch_policy and the complete rule inventory below.
                else:
                    # PUT cannot safely round-trip custom protection integrations.
                    raise Error('Unsupported GitHub protection rule; preserve it and reconcile explicitly')
            custom = pages(self.run, self.endpoint(name) + '/deployment_protection_rules', 'custom_deployment_protection_rules')
            if custom:
                raise Error('Custom deployment protections present; preserve them and reconcile explicitly')
            rules = pages(self.run, self.endpoint(name) + '/deployment-branch-policies', 'branch_policies')
            branches = []
            for rule in rules:
                if (not isinstance(rule, dict) or rule.get('type') not in ('branch', 'tag')
                        or not isinstance(rule.get('name'), str)):
                    raise Error('Unknown GitHub deployment branch rule')
                branches.append(dict(id=positive_id(rule.get('id')), name=rule['name'], type=rule['type']))
            if len({v['id'] for v in branches}) != len(branches):
                raise Error('Duplicate GitHub branch rule identity')
            result[name] = dict(id=positive_id(data.get('id')), config=config, branches=sorted(branches, key=lambda v: v['id']))
        return result

    def target(self, name, original):
        config = copy.deepcopy(original['config']) if original else dict(wait_timer=0)
        config.update(reviewers=[self.reviewer] if name == 'production-approval' else [],
                      prevent_self_review=False, can_admins_bypass=False,
                      deployment_branch_policy=dict(protected_branches=False, custom_branch_policies=True))
        return config

    def plan(self, operation):
        path = self.root / (operation + '.json')
        if path.exists():
            raise Error('Environment operation already exists; review its exact ID')
        desired = {name: self.target(name, self.current[name]) for name in self.names}
        conflicts = [name for name in self.names if self.current[name] is not None
                     and not self.satisfied(self.current[name], desired[name])]
        ledger = dict(schema=1, operation=operation, binding=self.binding, before=self.current,
                      desired=desired, conflicts=conflicts, observed=self.current, pending=None,
                      reviewed=False, started=False)
        local.save(path, ledger)
        self.report(ledger)

    @staticmethod
    def satisfied(value, config):
        return (value is not None and value['config'] == config and len(value['branches']) == 1
                and value['branches'][0]['name'] == 'master' and value['branches'][0]['type'] == 'branch')

    @staticmethod
    def matches(actual, expected):
        # Only identities allocated by the pending CREATE may be unknown.
        actual = copy.deepcopy(actual)
        for name, value in expected.items():
            if value is None or actual.get(name) is None:
                continue
            if value['id'] is None:
                actual[name]['id'] = None
            for branch in value['branches']:
                if branch['id'] is None:
                    found = [v for v in actual[name]['branches'] if (v['name'], v['type']) == (branch['name'], branch['type'])]
                    if len(found) != 1:
                        return False
                    found[0]['id'] = None
            actual[name]['branches'].sort(key=lambda v: v['id'] or 0)
        return actual == expected

    def load(self, operation):
        if not re.fullmatch(r'[a-f0-9]{32}', operation or ''):
            raise Error('Invalid environment operation identity')
        ledger = local.private_read(self.root / (operation + '.json'))
        if (not isinstance(ledger, dict) or ledger.get('schema') != 1 or ledger.get('operation') != operation
                or ledger.get('binding') != self.binding
                or set(ledger) != {'schema', 'operation', 'binding', 'before', 'desired', 'conflicts', 'observed', 'pending', 'reviewed', 'started'}
                or any(type(ledger[k]) is not bool for k in ('reviewed', 'started'))):
            raise Error('Environment operation is corrupt or belongs to another implementation/selection')
        # Verify the reviewed desired policy, not caller-supplied replacement state.
        if (set(ledger['before']) != set(self.names)
                or ledger['desired'] != {n: self.target(n, ledger['before'][n]) for n in self.names}
                or ledger['conflicts'] != [n for n in self.names if ledger['before'][n] is not None
                                           and not self.satisfied(ledger['before'][n], ledger['desired'][n])]):
            raise Error('Invalid reviewed GitHub environment policy')
        return ledger

    def report(self, ledger):
        print(json.dumps(dict(github_environments=dict(operation=ledger['operation'],
                              before=ledger['before'], desired=ledger['desired'],
                              existing_changes_require_approval=ledger['conflicts'])), sort_keys=True))

    def execute(self, operation, mode):
        ledger = self.load(operation)
        path = self.root / (operation + '.json')
        self.current = self.snapshot()
        if self.current != ledger['observed']:
            if ledger['started'] and ledger['pending'] and self.matches(self.current, ledger['pending']):
                if mode == 'apply':
                    ledger.update(observed=self.current, pending=None)
                    local.save(path, ledger)
            else:
                raise Error('GitHub environments changed since planning/checkpoint; replan, never silently overwrite')
        self.report(ledger)
        if mode == 'review':
            ledger['reviewed'] = True
            local.save(path, ledger)
            return
        if mode == 'reconcile':
            if not all(self.satisfied(self.current[n], ledger['desired'][n]) for n in self.names):
                raise Error('GitHub setup incomplete; review and explicitly resume the original apply')
            return
        if not ledger['reviewed']:
            raise Error('Review the exact environment operation before apply')
        if ledger['conflicts'] and not self.args.approve_environment_changes:
            raise Error('Existing GitHub protections differ; review then pass --approve-environment-changes')
        ledger['started'] = True
        local.save(path, ledger)

        def mutate(endpoint, method, body, expected):
            # Detect drift immediately before each effect, including between writes.
            if self.snapshot() != self.current:
                raise Error('GitHub environment changed before mutation; reconcile explicitly')
            ledger['pending'] = expected
            local.save(path, ledger)
            api(self.run, endpoint, method, body)
            observed = self.snapshot()
            if not self.matches(observed, expected):
                raise Error('GitHub mutation could not be verified; resume/reconcile exact operation')
            self.current = observed
            ledger.update(observed=observed, pending=None)
            local.save(path, ledger)

        for name in self.names:
            value = self.current[name]
            desired = ledger['desired'][name]
            if value is None or value['config'] != desired:
                expected = copy.deepcopy(self.current)
                expected[name] = dict(id=value['id'] if value else None, config=desired,
                                      branches=value['branches'] if value else [])
                mutate(self.endpoint(name), 'PUT', desired, expected)
            branches = self.current[name]['branches']
            master = next((b for b in branches if b['name'] == 'master' and b['type'] == 'branch'), None)
            if master is None:
                expected = copy.deepcopy(self.current)
                expected[name]['branches'].insert(0, dict(id=None, name='master', type='branch'))
                mutate(self.endpoint(name) + '/deployment-branch-policies', 'POST', dict(name='master', type='branch'), expected)
                master = next(b for b in self.current[name]['branches'] if b['name'] == 'master' and b['type'] == 'branch')
            for branch in list(self.current[name]['branches']):
                if branch['id'] != master['id']:
                    expected = copy.deepcopy(self.current)
                    expected[name]['branches'].remove(branch)
                    mutate(self.endpoint(name) + '/deployment-branch-policies/' + str(branch['id']), 'DELETE', None, expected)
        if self.snapshot() != self.current or not all(self.satisfied(self.current[n], ledger['desired'][n]) for n in self.names):
            raise Error('Final GitHub environment verification failed; do not provision secrets')
