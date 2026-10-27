# Delete a Group

Any group owner or Thorium admin can delete a group. It is important to remember that *groups* are the resource owners rather than the users that created those resources. Files, repositories, images, pipelines, tool results, and file comments can all be owned by groups. If you delete a group, some of these resources will automatically be purged by the API. For performance reasons, some resources will not be cleaned up when the API services a group deletion request. The following table indicates which resources are cleaned up automatically:

| Group Resources | Removed Upon Group Deletion |
| --- | ---------- |
| Files | No|
| File Comments | No |
| Images | Yes |
| Pipelines | Yes |
| Tool Results | No |
| Reactions | Yes |
| Reaction Stage Logs | No, aged out automatically |
| Repositories | No |
| Tools, Tool Instances, Agents, Toolsets, Workflows, Model Endpoints, Model Profiles, Model Credentials | No |

### Archiving Groups Instead
---
Generally we recommend archiving rather than deleting groups. You can do this by adding the Thorium system user to the group as an owner (since groups must have atleast 1 owner) and then removing non-admin group members. This preserves data and analysis artifacts without orphaning data and and mitigates the risk of future data leakage if that group name was reused by another team. 


### Preparing For Group Deletion
---
If you do want to delete a group, you will need to manually delete any files, repositories, tool results, and file comments using Thorctl or direct API requests. The following support table details what interfaces support deleting resources:

| Group Resources | Thorctl Deletion | API Deletion Route |
| --- | ---------- | ---------- |
| Files | `thorctl files delete` | `DELETE /api/files/sample/{sha256}/{submission}` |
| File Comments | No | `DELETE /api/files/comment/{sha256}/{id}` |
| Images | `thorctl images delete` | `DELETE /api/images/{group}/{image}` |
| Pipelines | `thorctl pipelines delete` | `DELETE /api/pipelines/{group}/{pipeline}` |
| Tool Results | No | Yes |
| Reactions | `thorctl reactions delete` | `DELETE /api/reactions/{group}/{id}` |
| Reaction Stage Logs | Deleted with the reaction | Deleted with the reaction |
| Repositories | No | Yes |
| Tools, Tool Instances, Agents, Toolsets, Workflows | `thorctl tools delete`, `agents delete`, `toolsets delete`, `workflows delete` (a tool with instances can't be deleted) | `DELETE /api/entities/{id}` |
| Model Endpoints, Model Profiles, Model Credentials | `thorctl models endpoints delete`, `profiles delete`, `credentials delete` | `DELETE /api/entities/{id}` |

### Manually Deleting Files
---
When you request to delete a file, you are deleting a file submission from a database. A file can have many different submissions from one or more groups. Therefore, a file will only be deleted from the backend object store when the last submission for a file is deleted. This means that a file can be safely "deleted" from one group without removing that file from other groups.

File submissions can be deleted in Thorctl or through direct API requests. When using Thorctl to delete files in bulk it is important to specify a group to limit the deletion operation to using the `-g` flag. You must also use the `--force` flag when not limiting the deletion to a specific target sha256/tag, because this is considered an especially dangerous operation.

**DANGER: always specify a group using the `-g` flag, otherwise you may delete files indiscriminately.**

```bash
$ thorctl files delete -g demo-group1234 --force
SHA256                                                           | SUBMISSION                          
-----------------------------------------------------------------+--------------------------------------
3d95783f81e84591dfe8a412c8cec2f5cfcbcbc45ede845bd72b32469e16a34b | 49e8a48b-8ba6-427c-96a9-02a4a9e5ff78 |
...
```
