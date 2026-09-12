from py_clob_client_v2 import ClobClient
import os

from dotenv import load_dotenv
load_dotenv("/home/ec2-user/hummingbird_rust/.env")  # or Path(__file__).resolve().parent.parent / ".env"

client = ClobClient(
    host="https://clob.polymarket.com",
    chain_id=137,  # Polygon mainnet
    key=os.getenv("ETH_PRIV_KEY")
)

# Creates new credentials or derives existing ones
credentials = client.create_or_derive_api_key()

print(credentials)
# {
#     "apiKey": "550e8400-e29b-41d4-a716-446655440000",
#     "secret": "base64EncodedSecretString",
#     "passphrase": "randomPassphraseString"
# }