# Automation relationships

Panoptes builds static relationships from YAML syntax and files in the indexed
repository. It does not execute automation, render templates, fetch dependencies,
or consult a running controller. These relationships describe possible dependencies;
conditions, tags, and runtime execution order can determine which ones actually run.

## Ansible

Playbooks and conventional role/task files expose named `play`, `task`, and
`handler` symbols alongside their YAML keys. Unnamed tasks use their source line.

- `import_playbook`, `import_tasks`, `include_tasks`, `include_vars`, and
  `vars_files` link to local YAML files. Short module names and the
  `ansible.builtin`/`ansible.legacy` forms are recognized, including `file:` mappings.
- Play `roles`, `include_role`, `import_role`, and role `meta/main.yml`
  dependencies link to the selected tasks, handlers, vars, defaults, and metadata
  entry points. `tasks_from`, `handlers_from`, `vars_from`, and `defaults_from`
  select alternate files. Local roles and fully qualified roles under
  `collections/ansible_collections/<namespace>/<collection>/roles/` are supported.
- Tasks inside `block`, `rescue`, and `always` retain their parent task/play.
  `notify` links to visible handlers by name, `role : handler`, or `listen` topic.
  Topics can reach multiple handlers. Separate plays retain separate visibility;
  task files inherit visibility from the plays that import them.

Imports use `imports` edges; notifications use `calls` edges, so they are available
through the existing `callers` tool in either direction. For example:

```sh
panoptes callers 'handler: Restart nginx' --path .
panoptes callers 'play: Configure web' --direction out --depth all --path .
```

Resolution follows conventional repository-local paths. Custom `roles_path`,
installed collections, inventory precedence, Jinja variable evaluation, YAML merge
expansion, and dynamically computed paths are not interpreted. Extensionless YAML
and Jinja templates are not indexed. Duplicate handler names remain unresolved
instead of guessing Ansible's last-loaded handler. Static dependencies do not
imply that an included role has executed before a notification.

See the Ansible documentation for [roles and entry points](https://docs.ansible.com/projects/ansible/latest/playbook_guide/playbooks_reuse_roles.html)
and [file search paths](https://docs.ansible.com/projects/ansible/latest/playbook_guide/playbook_pathing.html).

## GitHub Actions

Workflow files under `.github/workflows/` expose `job: <id>` and
`step: <job>.<id>` symbols. Action metadata (`action.yml` or `action.yaml`) exposes
composite `step: <id>` symbols. Steps without an `id` use `#1`, `#2`, and so on;
their names and source remain searchable.

- `needs` links dependent jobs to their prerequisites in the same workflow.
  Workflows call their jobs, and jobs/composites call their steps, allowing
  transitive traversal through execution structure.
- Job-level `uses` links to local reusable workflows. Step-level `uses` links to
  local action metadata, including recursively nested composites. Local `uses`
  paths are relative to the repository root. Remote actions, reusable workflows,
  and `docker://` images remain external dependency nodes with the complete ref.
- Expression references to `steps`, `needs`, `jobs`, and declared `inputs` link to
  their producers. Step IDs are scoped to their job or composite. Job, reusable
  workflow, and composite output definitions connect to the referenced producer.
  Reusable-job output references connect to the calling job.
- JavaScript action `runs.main`, `runs.pre`, and `runs.post` link to indexed entry
  files relative to the action metadata directory.

```sh
panoptes callers 'job: deploy' --direction out --depth all --path .
panoptes callers 'step: build.package' --direction out --depth all --path .
panoptes callers '.github/actions/setup/action.yml' --path .
```

Only `${{ ... }}` interpolations and direct job/step `if` expressions are inspected.
Static dot access and quoted bracket access (`steps['build'].outputs.version`)
are supported. Comments, expression string literals, and arbitrary YAML `uses`
keys do not create dependencies. Ambiguous IDs and dynamic indexed targets remain
unresolved. Matrix expansion, checkout path/ref changes, shell command execution,
remote source retrieval, and YAML merge expansion are not modeled. Local action
resolution assumes the indexed repository is checked out at the workspace root.

See GitHub's [workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax),
[action metadata](https://docs.github.com/en/actions/reference/workflows-and-actions/metadata-syntax),
and [expression contexts](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts).

## Docker Compose

Compose files expose named services, networks, volumes, configs, and secrets.
Service dependencies include `depends_on`, `links`, `volumes_from`, and
`service:` references in `network_mode`, `ipc`, and `pid`. Short and long volume,
config, secret, and network syntax links to declared resources. Bind mounts and
anonymous volumes are not mistaken for named volumes.

Local `include` files extend the names visible to the including configuration;
`extends` links to the named service in the specified file. Unrelated Compose
projects never participate in name lookup. Config/secret `file` references link
when the target is an indexed source file. No secret values are read specially.

Environment interpolation, implicit default networks, include `project_directory`
overrides, CLI-selected override files, and runtime profile selection are not
interpreted. Duplicate resource names in included files remain unresolved rather
than guessing a merged model. Included files do not inherit their caller's names.

```sh
panoptes callers 'service: db' --path .
panoptes callers 'volume: data' --path .
```

See the [Compose services reference](https://docs.docker.com/reference/compose-file/services/)
and [include rules](https://docs.docker.com/reference/compose-file/include/).
