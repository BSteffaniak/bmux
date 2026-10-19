"""Read-only admission checks for environments owned by infra/live-github."""
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


def api(run, endpoint):
    raw = run(['gh', 'api', endpoint])
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

    def verify(self):
        """Read-only admission check; all mutations belong to infra/live-github."""
        if not all(self.satisfied(self.current[name], self.target(name, self.current[name])) for name in self.names):
            raise Error('GitHub environments differ from release policy; plan/apply infra/live-github first')

    @staticmethod
    def satisfied(value, config):
        return (value is not None and value['config'] == config and len(value['branches']) == 1
                and value['branches'][0]['name'] == 'master' and value['branches'][0]['type'] == 'branch')
