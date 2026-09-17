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
