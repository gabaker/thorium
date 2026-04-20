import json

f = open("/tmp/thorium/result-files/magika.json", "r")
result = json.loads(f.read())[0]["result"]
status = result["status"]
if status == "ok":
    tags = {"FileType": result["value"]["output"]["label"]}
    open("/tmp/thorium/tags", "w").write(json.dumps(tags))
else:
    print("Could not extract file type:", status)