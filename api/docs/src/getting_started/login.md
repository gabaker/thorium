# Logging Into Thorium

Most tasks in Thorium require you to be authenticated. Thorctl requires occasional reauthentication as your token
expires.

## Thorctl
---

To login with Thorctl you will first need to download the executable. To download Thorctl, follow one of the guides
below based on your operating system type.

## Download

### Linux/Mac
Run this command on the Linux system that you want Thorctl to be installed on.
<script>
  let base = window.location.origin;
  document.write("<pre>");
  document.write("<code class=\"language-bash  hljs\">");
  document.write("curl " + base + "/api/binaries/install-thorctl.sh | bash -s -- " + base);
  document.write("</code>");
  document.write("</pre>");
</script>

### Windows
Download Thorctl from this [Windows Thorctl](../../../binaries/windows/x86-64/thorctl.exe) link.

## Login

After you have downloaded Thorctl you can authenticate to Thorium by running:

<script>
  document.write("<pre>");
  document.write("<code class=\"language-bash  hljs\">");
  document.write("thorctl login " + base);
  document.write("</code>");
  document.write("</pre>");
</script>

Enter your username and password when prompted and you should get a success message
as shown below:

<video autoplay loop controls>
  <source src="../static_resources/thorctl-login.mp4" type="video/mp4">
</video>

For scripts and CI jobs, pass `-u/--user` and `-p/--password` so nothing is prompted (the password is then visible to
other users of the machine, so use a `--keys` file or config where you can). See [Command Line Tool](./thorctl.md#login-via-thorctl)
for the config file and the certificate options.

## API
---

`POST /api/users/auth` with your username and password as basic authentication returns a token. Send it on the other
routes as `Authorization: Bearer <BASE64 TOKEN>`: the header carries the **base64 encoding** of the token, not the token
itself. A bad password is refused with `401`.

```bash
# get a token, then encode it once for the header (the examples in this guide use $TOKEN_B64)
curl -X POST -u alice https://<URL>/api/users/auth
export TOKEN_B64=$(printf %s '<TOKEN>' | base64 | tr -d '\n')
curl -H "Authorization: Bearer $TOKEN_B64" https://<URL>/api/users/whoami
```
