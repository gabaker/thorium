# Network Policies

Thorctl provides helpful commands to create, delete, and update network policies in a Thorium instance.
You can find a list of those commands by running `thorctl network-policies --help` (or, alternatively,
`thorctl netpols --help`).

## Deleting a Network Policy

`thorctl netpols delete <NAME>` deletes a network policy (pass `--id <ID>` when more than one policy has the same
name). A network policy is shared by every group it is in, so deleting it removes it for all of them. Thorctl
therefore reads the policy first and asks about it by name, naming the groups that hold it:

```
$ thorctl netpols delete allow-dns
Delete network policy 'allow-dns' (groups: corn, wheat, id: 6e1c...) for all of its groups? [y/N]
```

The default answer is no; answering no prints `Cancelled; nothing was changed.` and exits `0`. Pass
`--skip-confirm` (alias `--yes`) to delete without asking. Without a terminal and without the flag the command
exits `1`, naming the flag, and deletes nothing.

## Creating a Network Policy

To create a network policy, use the `thorctl netpols create` command:

```
$ thorctl netpols create --help
Create a network policy in Thorium

Usage: thorctl network-policies create [OPTIONS] --name <NAME> --groups <GROUPS> --rules-file <RULES_FILE>

Options:
  -n, --name <NAME>              The name of the network policy
  -g, --groups <GROUPS>          The groups to add this network policy to
  -f, --rules-file <RULES_FILE>  The path to the JSON/YAML file defining the network policy's rules
      --format <FORMAT>          The format the network policy rules file is in [default: yaml] [possible values:
                                 yaml, json]
      --forced                   Sets the policy to be forcibly applied to all images in its group(s)
      --default                  Sets the policy to be a default policy for images in its group(s), added to new
                                 images when no other policies are given
  -h, --help                     Print help
```

You can set the name and groups of the network policy using the `--name` and `--groups` flags (note that
multiple groups can be delimited with a `,`):

```
thorctl netpols create --name my-policy --groups crabs,corn ...
```

### The Rules File

The actual content of the network policy is defined in a "rules file", a YAML or JSON-formatted list
of rules the network policy should have. You can use the template network policy files below
for reference. For more information on accepted values for each field in the rules file, see the [Network Policy Rules Schema](./network_policies.md#rules)
An `allowed_tools` entry is a tool's id, a tool instance's id, or a tool's exact name. A name selects
whatever tool in the group holds that name, so use ids for security boundaries (see
[Tool Entries](./network_policies.md#tool-entries)).

rules-file.yaml:

```YAML
ingress:
  - allowed_ips:
    - cidr: 10.10.10.10
    - cidr: 10.10.0.0/16
      except:
        - 10.10.5.0/24
        - 10.10.6.0/24
    allowed_groups:
      - crabs
      - corn
    allowed_tools:
      - harvester
      - analyzer
    allowed_local: false
    allowed_internet: false
    allowed_all: false
    ports:
      - port: 1000
        end_port: 1006
        protocol: TCP
    allowed_custom:
      - namespace_labels:
        - key: ns-key1
          value: ns-value1
        - key: ns-key2
          value: ns-value2
      - pod_labels:
        - key: pod-key1
          value: pod-value1
        - key: pod-key2
          value: pod-value2
      - namespace_labels:
        - key: ns-with-pod-key
          value: ns-with-pod-value
        pod_labels:
        - key: pod-with-ns-key
          value: pod-with-ns-value
      - <MORE CUSTOM RULES>
  - <MORE RULES>
egress:
  - <EGRESS RULES>
```

rules-file.json:

```JSON
{
  "ingress": [
    {
      "allowed_ips": [
        {
          "cidr": "10.10.10.10",
        },
        {
          "cidr": "10.10.0.0/16",
          "except": [
            "10.10.5.0/24",
            "10.10.6.0/24"
          ]
        }
      ],
      "allowed_groups": [
        "crabs",
        "corn"
      ],
      "allowed_tools": [
        "harvester",
        "analyzer"
      ],
      "allowed_local": false,
      "allowed_internet": false,
      "allowed_all": false,
      "ports": [
        {
          "port": 1000,
          "end_port": 1006,
          "protocol": "TCP"
        }
      ],
      "allowed_custom": [
        {
          "namespace_labels": [
            {
              "key": "ns-key1",
              "value": "ns-value1"
            },
            {
              "key": "ns-key2",
              "value": "ns-value2"
            }
          ],
        },
        {
          "pod_labels": [
            {
              "key": "pod-key1",
              "value": "pod-value1"
            },
            {
              "key": "pod-key2",
              "value": "pod-value2"
            }
          ]
        },
        {
          "namespace_labels": [
            {
              "key": "ns-plus-pod-key",
              "value": "ns-plus-pod-value"
            }
          ],
          "pod_labels": [
            {
              "key": "pod-plus-ns-key",
              "value": "pod-plus-ns-value"
            }
          ]
        }
      ]
    }
  ],
  "egress": []
}
```

#### Ingress/Egress: Missing Vs. Empty

Note the subtle difference between a missing ingress/egress section and providing a
section explicitly with no rules.

If an ingress/egress section is missing, the created network policy **will have no effect on traffic in that direction.** For example, let's create a network policy
with the following rules file:

```YAML
ingress:
  - allowed_all: true
```

The above network policy will allow all traffic on ingress, but has no bearing on egress traffic
whatsoever. It won't restrict egress traffic, nor will it allow any egress traffic if egress is
restricted by another network policy.

Conversely, if an ingress/egress section is provided but has no rules, the network policy
**will restrict all traffic in that direction.** Let's change the rules file above to restrict
all traffic on egress but not affect ingress:

```YAML
egress:
```

The egress section was provided but has no rules, so all egress traffic will be restricted.
The ingress section was skipped entirely, so ingress traffic will not be affected by this network
policy.

And by this logic, we can provide an empty list of rules for ingress and egress to restrict
all traffic in both directions:

YAML:

```YAML
ingress:
egress:
```

JSON:

```JSON
{
  "ingress": [],
  "egress": [],
}
````

#### No Rules File or Empty Rules Files

What if we give a rules file that is missing both ingress _and_ egress, or we don't provide a
rules file at all? In that case, the resulting network policy **will restricting all traffic on
ingress and not affecting egress at all.** So an empty rules file has the same behavior as this
one:

```YAML
ingress:
```

This nuance is due to Kubernetes default behavior for created network policies. From
[Kubernetes Network Policies docs](https://kubernetes.io/docs/concepts/services-networking/network-policies/#networkpolicy-resource):

> "If no policyTypes are specified on a NetworkPolicy then by default Ingress will always
> be set and Egress will be set if the NetworkPolicy has any egress rules."


