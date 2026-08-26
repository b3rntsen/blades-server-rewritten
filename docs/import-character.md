# How to import a character for a user using the command from admin.rs (import-character)

1. export ARENA_IMPORT_TOKEN ( eg. export ARENA_IMPORT_TOKEN="smelly-camel" )

2. Send command:
	curl -i -X POST \
  -H "Content-Type: application/json" \
  -H "X-Import-Token: $ARENA_IMPORT_TOKEN" \
  --data-binary @<your-character>.json \
  http://127.0.0.1:8000/api/dev/v1/import-character

3. You should receive HTTP/1.1 200 OK --> it worked