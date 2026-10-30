"""Offline integrity checks: these tests never contact an LLM endpoint."""
import contextlib
import importlib.util
import io
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts/benchmark-agent-llm.py'
spec = importlib.util.spec_from_file_location('benchmark_agent_llm', SCRIPT)
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class BenchmarkIntegrityTests(unittest.TestCase):
    def git_fixture(self, root, remote='https://github.com/pocketbase/pocketbase.git', missing=None, leakage=False):
        revision = 'f' * 40
        def output(source, *args):
            self.assertEqual(source, os.path.realpath(root))
            if args == ('rev-parse', '--is-inside-work-tree'):
                return 'true'
            if args == ('rev-parse', '--show-toplevel'):
                return root
            if args == ('rev-parse', 'HEAD'):
                return revision
            if args == ('remote', 'get-url', 'origin'):
                return remote
            if args == ('ls-tree', '-r', '--name-only', revision):
                return '\n'.join(bench.POCKETBASE_ANCHORS) + ('\nbench/agent-tasks/pocketbase.tsv' if leakage else '')
            if args[0] == 'show':
                path = args[1].split(':', 1)[1]
                return '' if missing == path else ' '.join(bench.POCKETBASE_ANCHORS[path])
            raise AssertionError(args)
        return output

    def test_valid_origin_and_committed_evidence_record_actual_revision(self):
        with tempfile.TemporaryDirectory() as root:
            with mock.patch.object(bench, 'git_output', side_effect=self.git_fixture(root)), contextlib.redirect_stderr(io.StringIO()):
                facts = bench.validate_source(root, bench.DEFAULT_TASKS)
            self.assertEqual(facts['source_revision'], 'f' * 40)
            self.assertEqual(facts['source_repository'], 'pocketbase/pocketbase')
            self.assertEqual(facts['source_matches_configured_revision'], 'false')
            self.assertIn('committed task evidence', facts['source_validation'])

    def test_wrong_origin_missing_evidence_and_task_metadata_are_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            cases = [dict(remote='https://github.com/wallentx/panoptes.git'),
                     dict(missing='apis/record_helpers.go'), dict(leakage=True)]
            for options in cases:
                with self.subTest(options=options), mock.patch.object(bench, 'git_output', side_effect=self.git_fixture(root, **options)):
                    with self.assertRaises(ValueError):
                        bench.validate_source(root, bench.DEFAULT_TASKS)

    def test_custom_tasks_can_use_another_corpus_without_false_pocketbase_claim(self):
        with tempfile.TemporaryDirectory() as root:
            with mock.patch.object(bench, 'git_output', side_effect=self.git_fixture(root, remote='/local/custom')):
                facts = bench.validate_source(root, '/custom/tasks.tsv')
            self.assertEqual(facts['source_repository'], 'custom/local repository')
            self.assertTrue(facts['source_validation'].startswith('custom tasks:'))

    def test_wrong_corpus_stops_before_credentials_or_endpoint(self):
        arguments = [str(SCRIPT), '--source', '/mislabeled/pocketbase', '--binary', sys.executable]
        with mock.patch.object(sys, 'argv', arguments), mock.patch.object(bench, 'validate_source', side_effect=ValueError('wrong corpus')), mock.patch.object(bench, 'load_credentials') as credentials, mock.patch.object(bench, 'Endpoint') as endpoint, contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as error:
                bench.main()
            self.assertEqual(error.exception.code, 2)
            credentials.assert_not_called()
            endpoint.assert_not_called()

    def test_canonical_origin_strips_credentials_and_rejects_other_hosts(self):
        for remote in ['https://github.com/pocketbase/pocketbase.git', 'git@github.com:pocketbase/pocketbase.git', 'ssh://git@github.com/pocketbase/pocketbase.git', 'https://user:password@github.com/pocketbase/pocketbase.git']:
            self.assertEqual(bench.canonical_github_repository(remote), 'pocketbase/pocketbase')
        self.assertIsNone(bench.canonical_github_repository('https://github.com.evil.invalid/pocketbase/pocketbase'))

    def test_git_probe_ignores_inherited_git_directory(self):
        with mock.patch.dict(os.environ, {'GIT_DIR': '/wrong/repo', 'GIT_WORK_TREE': '/wrong/tree'}), mock.patch.object(bench.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, stdout='abc\n')) as run:
            self.assertEqual(bench.git_output('/correct/repo', 'rev-parse', 'HEAD'), 'abc')
            self.assertNotIn('GIT_DIR', run.call_args.kwargs['env'])
            self.assertNotIn('GIT_WORK_TREE', run.call_args.kwargs['env'])

    def test_read_file_retains_line_separators_and_existing_budget(self):
        with tempfile.TemporaryDirectory() as root:
            Path(root, 'file.txt').write_text('alpha\nbeta\ngamma\n')
            self.assertEqual(bench.tool_read_file(root, {'path': 'file.txt', 'offset': 2, 'limit': 2}, None), '2: beta\n3: gamma')
            self.assertEqual(bench.MAX_READ_LINES, 2000)

    def test_directory_tools_support_aliased_worktree_roots(self):
        with tempfile.TemporaryDirectory() as temp:
            parent = Path(temp)
            actual = parent / "private" / "actual"
            root = actual / "worktree"
            root.mkdir(parents=True)
            (root / "inside.txt").write_text("SAFE_ROOT_MARKER\n", encoding="utf-8")
            (parent / "alias").symlink_to(actual, target_is_directory=True)
            aliased = parent / "alias" / "worktree"
            grep = bench.tool_grep_files(str(aliased), {"pattern":"SAFE_ROOT_MARKER"}, None)
            self.assertEqual(grep, "inside.txt:1:SAFE_ROOT_MARKER")
            self.assertEqual(bench.tool_glob_files(str(aliased), {"pattern":"*.txt"}, None), "inside.txt")
            self.assertIn("SAFE_ROOT_MARKER", bench.tool_read_file(str(aliased), {"path":"inside.txt"}, None))

    def test_directory_tools_skip_escaping_symlinks_and_keep_internal_file_aliases(self):
        if not hasattr(os, 'symlink'):
            self.skipTest('symlinks are not supported')
        with tempfile.TemporaryDirectory() as base:
            corpus = Path(base, 'corpus'); corpus.mkdir()
            outside = Path(base, 'host'); outside.mkdir()
            secret = 'PRIVATE_HOST_CONTENT_DO_NOT_EXPOSE'
            (outside / 'secret.txt').write_text(secret + '\n')
            (corpus / 'real.txt').write_text('ALLOWED_CORPUS_CONTENT\n')
            (corpus / 'inside.txt').symlink_to(corpus / 'real.txt')
            (corpus / 'escape.txt').symlink_to(outside / 'secret.txt')
            (corpus / 'escape-dir').symlink_to(outside, target_is_directory=True)
            grep = bench.tool_grep_files(str(corpus), {'pattern': 'CONTENT|EXPOSE'}, None)
            self.assertIn('ALLOWED_CORPUS_CONTENT', grep)
            self.assertIn('inside.txt:1:', grep)
            self.assertNotIn(secret, grep)
            self.assertNotIn('escape', grep)
            glob = bench.tool_glob_files(str(corpus), {'pattern': '*.txt'}, None)
            self.assertIn('inside.txt', glob.splitlines())
            self.assertNotIn('escape.txt', glob.splitlines())
            self.assertNotIn('escape-dir/secret.txt', glob.splitlines())
            for path in ('escape.txt', 'escape-dir', str(outside / 'secret.txt')):
                with self.subTest(path=path):
                    result = bench.tool_grep_files(str(corpus), {'pattern': '.', 'path': path}, None)
                    self.assertNotIn(secret, result)
                    self.assertTrue(result.startswith('error:') or result == '(no matches)', result)
                    result = bench.tool_glob_files(str(corpus), {'pattern': '*', 'path': path}, None)
                    self.assertNotIn(secret, result)
                    self.assertTrue(result.startswith('error:') or result == '(no matches)', result)

    def test_directory_aliases_keep_display_paths_and_git_metadata_is_private(self):
        if not hasattr(os, 'symlink'):
            self.skipTest('symlinks are not supported')
        with tempfile.TemporaryDirectory() as base:
            corpus = Path(base, 'corpus'); corpus.mkdir()
            actual = corpus / 'directory'; actual.mkdir()
            (actual / 'child.txt').write_text('VISIBLE_DIRECTORY_CONTENT\n')
            (corpus / 'directory-alias').symlink_to(actual, target_is_directory=True)
            git = corpus / '.git'; git.mkdir()
            credential = 'FAKE_GIT_CONFIG_CREDENTIAL'
            (git / 'config').write_text(credential + '\n')
            (corpus / 'metadata-alias.txt').symlink_to(git / 'config')
            (corpus / 'metadata-dir').symlink_to(git, target_is_directory=True)
            result = bench.tool_grep_files(str(corpus), {'path': 'directory-alias', 'pattern': 'VISIBLE'}, None)
            self.assertIn('directory-alias/child.txt:1:VISIBLE_DIRECTORY_CONTENT', result)
            self.assertEqual(bench.tool_glob_files(str(corpus), {'path': 'directory-alias', 'pattern': '*.txt'}, None), 'directory-alias/child.txt')
            for path in ('.', '.git', '.git/config', 'metadata-alias.txt', 'metadata-dir'):
                with self.subTest(path=path):
                    result = bench.tool_grep_files(str(corpus), {'path': path, 'pattern': '.'}, None)
                    self.assertNotIn(credential, result)
                    self.assertNotIn(credential, bench.tool_read_file(str(corpus), {'path': path}, None))
            names = bench.tool_glob_files(str(corpus), {'pattern': '*'}, None).splitlines()
            self.assertNotIn('.git/config', names)
            self.assertNotIn('metadata-alias.txt', names)
            self.assertNotIn('metadata-dir/config', names)

    def test_credentials_resolve_each_field_in_cli_environment_file_order(self):
        token = 'OPENAI_BASE_URL=https://file.invalid/v1/\nOPENAI_API_KEY=file-key\n'
        cases = [
            # Explicit partial overrides must win even with a complete env pair.
            ('https://cli.invalid/v1/', None, {'OPENAI_BASE_URL': 'https://env.invalid/v1/', 'OPENAI_API_KEY': 'env-key'}, ('https://cli.invalid/v1', 'env-key'), False),
            (None, 'cli-key', {'OPENAI_BASE_URL': 'https://env.invalid/v1/', 'OPENAI_API_KEY': 'env-key'}, ('https://env.invalid/v1', 'cli-key'), False),
            # Missing env fields come from the file without discarding env values.
            (None, None, {'OPENAI_BASE_URL': 'https://env.invalid/v1/'}, ('https://env.invalid/v1', 'file-key'), True),
            (None, None, {'OPENAI_API_KEY': 'env-key'}, ('https://file.invalid/v1', 'env-key'), True),
            ('https://cli.invalid/v1/', None, {}, ('https://cli.invalid/v1', 'file-key'), True),
            (None, 'cli-key', {}, ('https://file.invalid/v1', 'cli-key'), True),
            (None, None, {}, ('https://file.invalid/v1', 'file-key'), True),
            ('https://cli.invalid/v1/', 'cli-key', {'OPENAI_BASE_URL': 'https://env.invalid/v1/', 'OPENAI_API_KEY': 'env-key'}, ('https://cli.invalid/v1', 'cli-key'), False),
            (None, None, {'OPENAI_BASE_URL': 'https://env.invalid/v1/', 'OPENAI_API_KEY': 'env-key'}, ('https://env.invalid/v1', 'env-key'), False),
            ('https://cli.invalid/v1/', None, {'OPENAI_API_KEY': 'env-key'}, ('https://cli.invalid/v1', 'env-key'), False),
            (None, 'cli-key', {'OPENAI_BASE_URL': 'https://env.invalid/v1/'}, ('https://env.invalid/v1', 'cli-key'), False),
        ]
        for cli_base, cli_key, environment, expected, reads_file in cases:
            args = bench.argparse.Namespace(base_url=cli_base, api_key=cli_key)
            opened = mock.mock_open(read_data=token)
            with self.subTest(cli_base=cli_base, cli_key=cli_key, environment=environment), mock.patch.dict(os.environ, environment, clear=True), mock.patch('builtins.open', opened):
                self.assertEqual(bench.load_credentials(args), expected)
                if reads_file:
                    opened.assert_called_once()
                else:
                    opened.assert_not_called()

    def test_mcp_function_names_are_api_compatible_and_dispatch_to_bare_tool_names(self):
        class Endpoint:
            model = 'offline'
            turn = 0
            def chat(self, messages, tools, reasoning_effort=None):
                for tool in tools:
                    self_test.assertRegex(tool['function']['name'], r'^[A-Za-z0-9_-]{1,64}$')
                self.turn += 1
                if self.turn == 1:
                    message = {'tool_calls': [{'id': 'mcp-1', 'function': {'name': 'panoptes_find', 'arguments': '{"query":"needle"}'}}]}
                    reason = 'tool_calls'
                else:
                    message = {'content': 'completed answer'}
                    reason = 'stop'
                return {'choices': [{'message': message, 'finish_reason': reason}], 'usage': {}}
        self_test = self
        with tempfile.TemporaryDirectory() as root, mock.patch.object(bench.subprocess, 'Popen'), mock.patch.object(bench.PanoptesMcp, '_initialize'):
            corpus = Path(root, 'corpus'); corpus.mkdir()
            client = bench.PanoptesMcp('/bin/panoptes', '/scratch/store.db', str(corpus))
            self.assertEqual(client.prefix, 'panoptes_')
            def send(method, params=None):
                if method == 'tools/list':
                    return {'tools': [{'name': 'find', 'description': 'find', 'inputSchema': {'type': 'object'}}]}
                self.assertEqual(method, 'tools/call')
                self.assertEqual(params, {'name': 'find', 'arguments': {'query': 'needle'}})
                return {'content': [{'type': 'text', 'text': 'evidence'}], 'isError': False}
            client._send = mock.Mock(side_effect=send)
            client.close = mock.Mock()
            with mock.patch.object(bench, 'args_max_turns', 3, create=True):
                metrics, answer = bench.run_one(Endpoint(), 'task', 'question', 'panoptes', 1, 'low', str(corpus), client, str(Path(root, 'events.jsonl')), str(Path(root, 'answer.md')))
            self.assertEqual(metrics['exit_code'], 0)
            self.assertEqual(metrics['panoptes_calls'], 1)
            self.assertEqual(answer, 'completed answer')
            client.close.assert_called_once()

    def test_incomplete_and_empty_final_responses_fail_but_keep_usage_and_evidence(self):
        cases = [('length', 'truncated answer'), ('content_filter', 'filtered answer'),
                 ('stop', ''), ('stop', '  \n\t'), ('stop', None),
                 (None, 'unconfirmed answer'), ('tool_calls', 'unconfirmed answer'), ('unexpected', 'unconfirmed answer')]
        for reason, content in cases:
            class Endpoint:
                model = 'offline'
                def chat(self, messages, tools, reasoning_effort=None):
                    return {'choices': [{'message': {'content': content}, 'finish_reason': reason}],
                            'usage': {'prompt_tokens': 10, 'completion_tokens': 2,
                                      'prompt_tokens_details': {'cached_tokens': 3},
                                      'completion_tokens_details': {'reasoning_tokens': 1}}}
            with self.subTest(reason=reason, content=content), tempfile.TemporaryDirectory() as root:
                corpus = Path(root, 'corpus'); corpus.mkdir()
                events = Path(root, 'events.jsonl'); answer = Path(root, 'answer.md')
                with mock.patch.object(bench, 'args_max_turns', 1, create=True):
                    metrics, result = bench.run_one(Endpoint(), 'task', 'question', 'baseline', 1, 'low', str(corpus), None, str(events), str(answer))
                self.assertNotEqual(metrics['exit_code'], 0)
                self.assertEqual((metrics['input_tokens'], metrics['cached_input_tokens'], metrics['output_tokens'], metrics['reasoning_output_tokens']), (10, 3, 2, 1))
                records = [json.loads(line) for line in events.read_text().splitlines()]
                self.assertTrue(any(r['type'] == 'run.error' for r in records))
                assistant = next(r for r in records if r['type'] == 'assistant')
                self.assertEqual(assistant['finish_reason'], reason)
                self.assertEqual(answer.read_text(), result)

    def test_truncated_tool_call_is_rejected_before_execution(self):
        class Endpoint:
            model = 'offline'
            def chat(self, messages, tools, reasoning_effort=None):
                return {'choices': [{'message': {'tool_calls': [{'id': 't', 'function': {'name': 'read_file', 'arguments': '{"path":"file.txt"}'}}]}, 'finish_reason': 'length'}],
                        'usage': {'prompt_tokens': 10, 'completion_tokens': 2}}
        with tempfile.TemporaryDirectory() as root:
            corpus = Path(root, 'corpus'); corpus.mkdir()
            events = Path(root, 'events.jsonl')
            with mock.patch.object(bench, 'args_max_turns', 1, create=True), mock.patch.object(bench, 'tool_read_file') as read:
                metrics, result = bench.run_one(Endpoint(), 'task', 'question', 'baseline', 1, 'low', str(corpus), None, str(events), str(Path(root, 'answer.md')))
            read.assert_not_called()
            self.assertNotEqual(metrics['exit_code'], 0)
            self.assertEqual(metrics['tool_calls'], 0)
            self.assertEqual((metrics['input_tokens'], metrics['output_tokens']), (10, 2))
            records = [json.loads(line) for line in events.read_text().splitlines()]
            self.assertTrue(any(r['type'] == 'run.error' for r in records))
            self.assertFalse(any(r['type'] == 'tool_result' for r in records))

    def test_complete_stop_response_is_successful(self):
        class Endpoint:
            model = 'offline'
            def chat(self, messages, tools, reasoning_effort=None):
                return {'choices': [{'message': {'content': ' completed answer '}, 'finish_reason': 'stop'}],
                        'usage': {'prompt_tokens': 7, 'completion_tokens': 3}}
        with tempfile.TemporaryDirectory() as root:
            corpus = Path(root, 'corpus'); corpus.mkdir()
            with mock.patch.object(bench, 'args_max_turns', 1, create=True):
                metrics, result = bench.run_one(Endpoint(), 'task', 'question', 'baseline', 1, 'low', str(corpus), None, str(Path(root, 'events.jsonl')), str(Path(root, 'answer.md')))
            self.assertEqual(metrics['exit_code'], 0)
            self.assertEqual(result, 'completed answer')
            self.assertEqual((metrics['input_tokens'], metrics['output_tokens']), (7, 3))

    def test_request_sizes_describe_exact_wire_json_without_credentials(self):
        messages = [{'role': 'user', 'content': 'é'}]
        tools = [{'type': 'function', 'function': {'name': 'example'}}]
        facts = bench.request_sizes('model', messages, tools, 'low')
        payload = json.dumps(bench.chat_payload('model', messages, tools, 'low'))
        self.assertEqual(facts['request_chars'], len(payload))
        self.assertEqual(facts['request_bytes'], len(payload.encode()))
        self.assertEqual(facts['schema_chars'], len(json.dumps(tools)))
        self.assertEqual(facts['message_chars'], len(json.dumps(messages)))
        self.assertNotIn('Authorization', payload)

    def test_offline_agent_events_record_sizes_and_elapsed_without_changing_usage(self):
        class Endpoint:
            model = 'offline'
            def __init__(self): self.turn = 0
            def chat(self, messages, tools, reasoning_effort=None):
                self.turn += 1
                if self.turn == 1:
                    message = {'tool_calls': [{'id': 'tool-1', 'function': {'name': 'read_file', 'arguments': '{"path":"file.txt","limit":2}'}}]}
                else:
                    message = {'content': 'answer'}
                return {'choices': [{'message': message, 'finish_reason': 'tool_calls' if self.turn == 1 else 'stop'}], 'usage': {'prompt_tokens': 10, 'completion_tokens': 2, 'prompt_tokens_details': {'cached_tokens': 3}}}
        with tempfile.TemporaryDirectory() as root:
            corpus = Path(root, 'corpus'); corpus.mkdir()
            Path(corpus, 'file.txt').write_text('a\nb\n')
            events = str(Path(root, 'events.jsonl')); answer = str(Path(root, 'answer.md'))
            with mock.patch.object(bench, 'args_max_turns', 3, create=True):
                metrics, result = bench.run_one(Endpoint(), 'task', 'question', 'baseline', 1, 'low', str(corpus), None, events, answer)
            self.assertEqual(result, 'answer')
            self.assertEqual((metrics['input_tokens'], metrics['cached_input_tokens'], metrics['output_tokens']), (20, 6, 4))
            records = [json.loads(line) for line in Path(events).read_text().splitlines()]
            requests = [r for r in records if r['type'] == 'request']
            self.assertEqual(len(requests), 2)
            self.assertGreater(requests[1]['message_chars'], requests[0]['message_chars'])
            tool = next(r for r in records if r['type'] == 'tool_result')
            self.assertEqual(tool['output'], '1: a\n2: b')
            self.assertGreaterEqual(tool['elapsed_ms'], 0)
            self.assertFalse(tool['output_truncated'])
            self.assertTrue(all(r['endpoint_elapsed_ms'] >= 0 for r in records if r['type'] == 'assistant'))


    def test_quick_footer_reads_cost_usage_and_rubric_from_result_rows(self):
        header = "task\tarm\ttrial\teffort\texit_code\testimated_cost_usd\tinput_tokens\tuncached_input_tokens\ttool_calls\trubric_hits\trubric_total\n"
        rows = ["auth\tbaseline\t1\tlow\t0\t0.2\t100\t40\t5\t6\t6\n",
                "auth\tpanoptes\t1\tlow\t0\t0.1\t80\t20\t3\t5\t6\n",
                "auth\tbaseline\t1\thigh\t0\t0\t0\t0\t0\t6\t6\n",
                "auth\tpanoptes\t1\thigh\t0\t0\t0\t0\t0\t6\t6\n"]
        with tempfile.TemporaryDirectory() as root:
            path = Path(root, 'results.tsv'); path.write_text(header + ''.join(rows))
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                bench.print_quick_results(path)
            text = output.getvalue()
            self.assertIn('cost $0.2000 vs $0.1000 (-50.0%)', text)
            self.assertIn('input 100 vs 80 (-20.0%)', text)
            self.assertIn('uncached 40 vs 20 (-50.0%)', text)
            self.assertIn('tool_calls 5 vs 3', text)
            self.assertIn('keyword coverage (not accuracy) 6/6 vs 5/6', text)
            self.assertIn('effort low', text)
            self.assertIn('effort high', text)
            self.assertIn('cost $0.0000 vs $0.0000 (n/a)', text)

    def test_quick_footer_does_not_pair_different_efforts(self):
        header = "task\tarm\ttrial\teffort\texit_code\testimated_cost_usd\tinput_tokens\tuncached_input_tokens\ttool_calls\trubric_hits\trubric_total\n"
        rows = "auth\tbaseline\t1\tlow\t0\t0.2\t100\t40\t5\t6\t6\nauth\tpanoptes\t1\thigh\t0\t0.1\t80\t20\t3\t5\t6\n"
        with tempfile.TemporaryDirectory() as root:
            path = Path(root, 'results.tsv'); path.write_text(header + rows)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                bench.print_quick_results(path)
            self.assertEqual(output.getvalue(), '')


    def test_quick_footer_excludes_incomplete_run_pairs(self):
        header = "task\tarm\ttrial\teffort\texit_code\testimated_cost_usd\tinput_tokens\tuncached_input_tokens\ttool_calls\trubric_hits\trubric_total\n"
        rows = "auth\tbaseline\t1\tlow\t0\t0.2\t100\t40\t5\t6\t6\nauth\tpanoptes\t1\tlow\t1\t0.1\t80\t20\t3\t5\t6\n"
        with tempfile.TemporaryDirectory() as root:
            path = Path(root, 'results.tsv'); path.write_text(header + rows)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                bench.print_quick_results(path)
            self.assertEqual(output.getvalue(), '')

    def test_compact_mcp_command_is_opt_in(self):
        for compact in (False, True):
            with self.subTest(compact=compact), mock.patch.object(bench.subprocess, 'Popen') as popen, mock.patch.object(bench.PanoptesMcp, '_initialize'):
                bench.PanoptesMcp('/bin/panoptes', '/scratch/store.db', '/scratch/corpus', compact=compact)
                expected = ['/bin/panoptes', '--store', '/scratch/store.db', 'mcp', '/scratch/corpus']
                if compact:
                    expected.append('--compact')
                self.assertEqual(popen.call_args.args[0], expected)


    def test_long_tool_sidecars_are_complete_hashed_and_outside_agent_workspace(self):
        full = "prefix\né" * 1000
        fake_credential = 'credential-must-not-enter-artifacts'
        class Endpoint:
            model = 'offline'
            api_key = fake_credential
            turn = 0
            def chat(self, messages, tools, reasoning_effort=None):
                self.turn += 1
                if self.turn == 1:
                    message = {'tool_calls': [{'id': 'tool-1', 'function': {'name': 'panoptes_../../unsafe', 'arguments': '{}'}}]}
                else:
                    message = {'content': 'answer'}
                return {'choices': [{'message': message, 'finish_reason': 'tool_calls' if self.turn == 1 else 'stop'}], 'usage': {}}
        class Mcp:
            prefix = 'panoptes_'
            def list_tools(self): return []
            def call(self, name, arguments): return full, False
            def close(self): pass
        with tempfile.TemporaryDirectory() as root:
            corpus = Path(root, 'corpus'); corpus.mkdir()
            events = Path(root, 'events.jsonl')
            with mock.patch.object(bench, 'args_max_turns', 3, create=True):
                bench.run_one(Endpoint(), 'task', 'question', 'panoptes', 1, 'low', str(corpus), Mcp(), str(events), str(Path(root, 'answer.md')))
            records = [json.loads(line) for line in events.read_text().splitlines()]
            self.assertEqual(records[0]['rubric_method'], 'keyword_presence_not_correctness')
            tool = next(r for r in records if r['type'] == 'tool_result')
            relative = Path(tool['output_path'])
            self.assertFalse(relative.is_absolute())
            self.assertNotIn('..', relative.parts)
            self.assertEqual(relative.name, '000001.txt')
            sidecar = events.parent / relative
            self.assertFalse(sidecar.is_relative_to(corpus))
            self.assertEqual(sidecar.read_text(), full)
            self.assertEqual(hashlib.sha256(sidecar.read_bytes()).hexdigest(), tool['output_sha256'])
            self.assertEqual(tool['output_chars'], len(full))
            self.assertEqual(tool['output_bytes'], len(full.encode()))
            self.assertEqual(tool['output'], full[:4000])
            self.assertTrue(tool['output_truncated'])
            self.assertNotIn(fake_credential, events.read_text())
            self.assertNotIn(fake_credential, sidecar.read_text())
            self.assertEqual(list(corpus.iterdir()), [])

    def test_evidence_inside_worktree_or_via_artifact_symlink_is_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            corpus = Path(root, 'corpus'); corpus.mkdir()
            with self.assertRaisesRegex(ValueError, 'outside the agent worktree'):
                bench.create_tool_output_store(str(corpus / 'events.jsonl'), str(corpus))
            if hasattr(os, 'symlink'):
                os.symlink(corpus, Path(root, 'tool-outputs'), target_is_directory=True)
                with self.assertRaisesRegex(ValueError, 'symlink'):
                    bench.create_tool_output_store(str(Path(root, 'events.jsonl')), str(corpus))


if __name__ == '__main__':
    unittest.main()
