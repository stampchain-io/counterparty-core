# What is a dispenser?
#
# A dispenser is a type of order where the holder address gives out a given amount
# of units of an asset for a given amount of BTC satoshis received.
# It's a very simple but powerful semantic to allow swaps to operate on-chain.
#
import json
import logging
import os
import struct
from math import floor

from counterpartycore.lib import (
    config,
    exceptions,
    ledger,
)
from counterpartycore.lib.ledger.currentstate import CurrentState
from counterpartycore.lib.parser import messagetype, protocol
from counterpartycore.lib.utils import address, helpers
from counterpartycore.lib.utils.address import pack_legacy as address_pack
from counterpartycore.lib.utils.address import unpack_legacy as address_unpack

logger = logging.getLogger(config.LOGGER_NAME)

FORMAT = ">QQQQB"
LENGTH = 33
ID = 12
DISPENSE_ID = 13

STATUS_OPEN = 0
STATUS_OPEN_EMPTY_ADDRESS = 1
# STATUS_OPEN_ORACLE_PRICE = 20
# STATUS_OPEN_ORACLE_PRICE_EMPTY_ADDRESS = 21
STATUS_CLOSED = 10
STATUS_CLOSING = 11

CURR_DIR = os.path.dirname(os.path.realpath(__file__))
with open(os.path.join(CURR_DIR, "data", "get_oldest_tx.json"), encoding="utf-8") as f:
    GET_OLDEST_TX_DATA = json.load(f)


def get_oldest_tx(source_address: str, block_index: int):
    key = f"{source_address}-{block_index}"
    if key in GET_OLDEST_TX_DATA:
        return GET_OLDEST_TX_DATA[key]
    return {}


def validate(
    db,
    source,
    asset,
    give_quantity,
    escrow_quantity,
    mainchainrate,
    status,
    open_address,
    block_index,
    oracle_address,
):
    problems = []
    asset_id = None

    if asset == config.BTC:
        problems.append(f"cannot dispense {config.BTC}")
        return None, problems

    # resolve subassets
    asset = ledger.issuances.resolve_subasset_longname(db, asset)

    if status in [STATUS_OPEN, STATUS_OPEN_EMPTY_ADDRESS]:
        if give_quantity <= 0:
            problems.append("give_quantity must be positive")
        if mainchainrate <= 0:
            problems.append("mainchainrate must be positive")
        if escrow_quantity < give_quantity:
            problems.append("escrow_quantity must be greater or equal than give_quantity")
    elif status != STATUS_CLOSED:
        problems.append(f"invalid status {status}")

    cursor = db.cursor()
    available = ledger.balances.get_balance(db, source, asset, return_list=True)

    if len(available) == 0:
        problems.append(f"address doesn't have the asset {asset}")
    elif len(available) >= 1 and available[0]["quantity"] < escrow_quantity:
        problems.append(
            f"address doesn't have enough balance of {asset} ({available[0]['quantity']} < {escrow_quantity})"
        )
    elif (
        protocol.enabled("dispenser_must_be_created_by_source")
        and open_address is not None
        and source != open_address
        and status != STATUS_CLOSED
        and len(
            ledger.markets.get_dispensers(
                db,
                status_in=[0, 11],
                address=open_address if status == STATUS_OPEN_EMPTY_ADDRESS else source,
                asset=asset,
            )
        )
        == 0
    ):
        problems.append("dispenser must be created by source")
    else:
        if status == STATUS_OPEN_EMPTY_ADDRESS and not open_address:
            open_address = source
            status = STATUS_OPEN

        # status == STATUS_OPEN_EMPTY_ADDRESS means open_address != source
        if (
            protocol.enabled("dispenser_must_be_created_by_source")
            and status == STATUS_OPEN_EMPTY_ADDRESS
            and open_address == source
        ):
            status = STATUS_OPEN

        open_dispensers = []
        if (
            protocol.enabled("dispenser_origin_permission_extended", block_index)
            and status == STATUS_CLOSED
            and open_address
            and open_address != source
        ):
            open_dispensers = ledger.markets.get_dispensers(
                db, status_in=[0, 11], address=open_address, asset=asset, origin=source
            )
        else:
            query_address = open_address if status == STATUS_OPEN_EMPTY_ADDRESS else source
            open_dispensers = ledger.markets.get_dispensers(
                db, status_in=[0, 11], address=query_address, asset=asset
            )

        if len(open_dispensers) == 0 or open_dispensers[0]["status"] != STATUS_CLOSING:
            if status in [STATUS_OPEN, STATUS_OPEN_EMPTY_ADDRESS]:
                if len(open_dispensers) > 0:
                    max_refills = protocol.get_value_by_block_index("max_refills", block_index)
                    refilling_count = 0
                    if max_refills > 0:
                        refilling_count = ledger.markets.get_refilling_count(
                            db, dispenser_tx_hash=open_dispensers[0]["tx_hash"]
                        )

                    # It's a valid refill
                    if (
                        open_dispensers[0]["satoshirate"] == mainchainrate
                        and open_dispensers[0]["give_quantity"] == give_quantity
                    ):
                        if refilling_count >= max_refills > 0:
                            problems.append("the dispenser reached its maximum refilling")
                    else:
                        if open_dispensers[0]["satoshirate"] != mainchainrate:
                            problems.append(
                                f"address has a dispenser already opened for asset {asset} with a different mainchainrate"
                            )
                        if open_dispensers[0]["give_quantity"] != give_quantity:
                            problems.append(
                                f"address has a dispenser already opened for asset {asset} with a different give_quantity"
                            )
            elif status == STATUS_CLOSED:
                if len(open_dispensers) == 0:
                    problems.append(f"address doesn't have an open dispenser for asset {asset}")

            if status == STATUS_OPEN_EMPTY_ADDRESS:
                # If an address is trying to refill a dispenser in a different address and it's the creator
                if not (
                    protocol.enabled("dispenser_origin_permission_extended", block_index)
                    and (len(open_dispensers) > 0)
                    and (open_dispensers[0]["origin"] == source)
                ):
                    dispensers_from_same_origin_count = ledger.markets.get_dispensers_count(
                        db, source=query_address, status=STATUS_CLOSED, origin=source
                    )

                    if not (
                        protocol.enabled("dispenser_origin_permission_extended", block_index)
                        and dispensers_from_same_origin_count > 0
                    ):
                        # It means that the same origin has not opened other dispensers in this address
                        existing_balances = ledger.balances.get_balances_count(db, query_address)

                        if existing_balances[0]["cnt"] > 0:
                            problems.append(
                                "cannot open on another address if it has any balance history"
                            )

                        if protocol.enabled("dispenser_origin_permission_extended", block_index):
                            address_oldest_transaction = get_oldest_tx(
                                query_address, block_index=CurrentState().current_block_index()
                            )
                            if (
                                ("block_index" in address_oldest_transaction)
                                and (address_oldest_transaction["block_index"] > 0)
                                and (block_index > address_oldest_transaction["block_index"])
                            ):
                                problems.append(
                                    "cannot open on another address if it has any confirmed bitcoin txs"
                                )

            if len(problems) == 0:
                asset_id = ledger.issuances.generate_asset_id(asset)
                if asset_id == 0:
                    problems.append(
                        f"cannot dispense {asset}"
                    )  # How can we test this on a test vector?
        else:
            problems.append(
                "address has already a dispenser about to close, no action can be taken until it closes"
            )

    cursor.close()

    if oracle_address is not None and protocol.enabled("oracle_dispensers", block_index):
        last_price, _last_fee, _last_label, _last_updated = ledger.other.get_oracle_last_price(
            db, oracle_address, block_index
        )

        if last_price is None:
            problems.append(
                f"The oracle address {oracle_address} has not broadcasted any price yet"
            )

    if (
        give_quantity > config.MAX_INT
        or escrow_quantity > config.MAX_INT
        or mainchainrate > config.MAX_INT
    ):
        problems.append("integer overflow")

    if len(problems) > 0:
        return None, problems
    return asset_id, None


def compose(
    db,
    source: str,
    asset: str,
    give_quantity: int,
    escrow_quantity: int,
    mainchainrate: int,
    status: int,
    open_address: str = None,
    oracle_address: str = None,
    skip_validation: bool = False,
):
    if oracle_address is not None and len(address.pack(oracle_address)) > 22:
        raise exceptions.ComposeError("Oracle address not supported by dispenser")

    asset_id, problems = validate(
        db,
        source,
        asset,
        give_quantity,
        escrow_quantity,
        mainchainrate,
        status,
        open_address,
        CurrentState().current_block_index(),
        oracle_address,
    )
    if problems and not skip_validation:
        raise exceptions.ComposeError(problems)
    if asset_id is None:
        asset_id = ledger.issuances.generate_asset_id(asset)
    destination = []
    data = messagetype.pack(ID)
    data += struct.pack(FORMAT, asset_id, give_quantity, escrow_quantity, mainchainrate, status)
    is_empty_address = status == STATUS_OPEN_EMPTY_ADDRESS and open_address
    if is_empty_address or (
        protocol.enabled("dispenser_origin_permission_extended")
        and status == STATUS_CLOSED
        and open_address
        and open_address != source
    ):
        data += address_pack(open_address)
    if oracle_address is not None and protocol.enabled("oracle_dispensers"):
        oracle_fee = calculate_oracle_fee(
            db,
            escrow_quantity,
            give_quantity,
            mainchainrate,
            oracle_address,
            CurrentState().current_block_index(),
        )

        if oracle_fee >= config.DEFAULT_REGULAR_DUST_SIZE:
            destination.append((oracle_address, oracle_fee))
        data += address_pack(oracle_address)

    return (source, destination, data)


def calculate_oracle_fee(
    db, escrow_quantity, give_quantity, mainchainrate, oracle_address, block_index
):
    last_price, last_fee, _last_fiat_label, _last_updated = ledger.other.get_oracle_last_price(
        db, oracle_address, block_index
    )
    last_fee_multiplier = last_fee / config.UNIT

    # Format mainchainrate to ######.##
    oracle_mainchainrate = helpers.satoshirate_to_fiat(mainchainrate)
    oracle_mainchainrate_btc = oracle_mainchainrate / last_price

    # Calculate the total amount earned for dispenser and the fee
    remaining = int(floor(escrow_quantity / give_quantity))
    total_quantity_btc = oracle_mainchainrate_btc * remaining
    oracle_fee_btc = int(total_quantity_btc * last_fee_multiplier * config.UNIT)

    return oracle_fee_btc


def unpack(message, return_dict=False):
    try:
        action_address = None
        oracle_address = None
        assetid, give_quantity, escrow_quantity, mainchainrate, dispenser_status = struct.unpack(
            FORMAT, message[0:LENGTH]
        )
        read = LENGTH
        if dispenser_status == STATUS_OPEN_EMPTY_ADDRESS or (
            protocol.enabled("dispenser_origin_permission_extended")
            and dispenser_status == STATUS_CLOSED
            and len(message) > read
        ):
            action_address = address_unpack(message[LENGTH : LENGTH + 21])
            read = LENGTH + 21
        if len(message) > read:
            oracle_address = address_unpack(message[read : read + 21])
        asset = ledger.issuances.generate_asset_name(assetid)
        status = "valid"
    except (exceptions.UnpackError, struct.error):
        (
            give_quantity,
            escrow_quantity,
            mainchainrate,
            dispenser_status,
            action_address,
            oracle_address,
            asset,
        ) = None, None, None, None, None, None, None
        status = "invalid: could not unpack"

    if return_dict:
        return {
            "asset": asset,
            "give_quantity": give_quantity,
            "escrow_quantity": escrow_quantity,
            "mainchainrate": mainchainrate,
            "dispenser_status": dispenser_status,
            "action_address": action_address,
            "oracle_address": oracle_address,
            "status": status,
        }
    return (
        asset,
        give_quantity,
        escrow_quantity,
        mainchainrate,
        dispenser_status,
        action_address,
        oracle_address,
        status,
    )


def parse(db, tx, message):
    cursor = db.cursor()

    # Unpack message.
    (
        asset,
        give_quantity,
        escrow_quantity,
        mainchainrate,
        dispenser_status,
        action_address,
        oracle_address,
        status,
    ) = unpack(message)
    if action_address is None:
        action_address = tx["source"]

    if status == "valid":
        if protocol.enabled("dispenser_parsing_validation", CurrentState().current_block_index()):
            _asset_id, problems = validate(
                db,
                tx["source"],
                asset,
                give_quantity,
                escrow_quantity,
                mainchainrate,
                dispenser_status,
                action_address
                if dispenser_status in [STATUS_OPEN_EMPTY_ADDRESS, STATUS_CLOSED]
                else None,
                tx["block_index"],
                oracle_address,
            )
        else:
            problems = None

        if problems:
            status = "invalid: " + "; ".join(problems)
        else:
            if dispenser_status in [STATUS_OPEN, STATUS_OPEN_EMPTY_ADDRESS]:
                existing = ledger.markets.get_dispensers(
                    db, address=action_address, asset=asset, status=STATUS_OPEN
                )

                if len(existing) == 0:
                    if oracle_address is not None and protocol.enabled(  # noqa: E711
                        "oracle_dispensers", tx["block_index"]
                    ):
                        oracle_fee = calculate_oracle_fee(
                            db,
                            escrow_quantity,
                            give_quantity,
                            mainchainrate,
                            oracle_address,
                            tx["block_index"],
                        )

                        if oracle_fee >= config.DEFAULT_REGULAR_DUST_SIZE:
                            if tx["destination"] != oracle_address or tx["btc_amount"] < oracle_fee:
                                status = "invalid: insufficient or non-existent oracle fee"

                    if status == "valid":
                        # Create the new dispenser
                        try:
                            if dispenser_status == STATUS_OPEN_EMPTY_ADDRESS:
                                is_empty_address = True
                                address_assets = ledger.balances.get_address_assets(
                                    db, action_address
                                )
                                if len(address_assets) > 0:
                                    for asset_name in address_assets:
                                        asset_balance = ledger.balances.get_balance(
                                            db, action_address, asset_name["asset"]
                                        )
                                        if asset_balance > 0:
                                            is_empty_address = False
                                            break

                                if is_empty_address:
                                    ledger.events.debit(
                                        db,
                                        tx["source"],
                                        asset,
                                        escrow_quantity,
                                        tx["tx_index"],
                                        action="open dispenser empty addr",
                                        event=tx["tx_hash"],
                                    )
                                    ledger.events.credit(
                                        db,
                                        action_address,
                                        asset,
                                        escrow_quantity,
                                        tx["tx_index"],
                                        action="open dispenser empty addr",
                                        event=tx["tx_hash"],
                                    )
                                    ledger.events.debit(
                                        db,
                                        action_address,
                                        asset,
                                        escrow_quantity,
                                        tx["tx_index"],
                                        action="open dispenser empty addr",
                                        event=tx["tx_hash"],
                                    )
                                else:
                                    status = "invalid: address not empty"
                            else:
                                ledger.events.debit(
                                    db,
                                    tx["source"],
                                    asset,
                                    escrow_quantity,
                                    tx["tx_index"],
                                    action="open dispenser",
                                    event=tx["tx_hash"],
                                )
                        except exceptions.DebitError:
                            status = "invalid: insufficient funds"

                    if status == "valid":
                        bindings = {
                            "tx_index": tx["tx_index"],
                            "tx_hash": tx["tx_hash"],
                            "block_index": tx["block_index"],
                            "source": action_address,
                            "asset": asset,
                            "give_quantity": give_quantity,
                            "escrow_quantity": escrow_quantity,
                            "satoshirate": mainchainrate,
                            "status": STATUS_OPEN,
                            "give_remaining": escrow_quantity,
                            "oracle_address": oracle_address,
                            "origin": tx["source"],
                            "dispense_count": 0,
                        }

                        if protocol.enabled("dispenser_origin_permission_extended"):
                            bindings["origin"] = tx["source"]

                        ledger.events.insert_record(db, "dispensers", bindings, "OPEN_DISPENSER")
                        # Add the address to the dispensable cache
                        if not CurrentState().parsing_mempool():
                            DispensableCache(db).new_dispensable(action_address)

                        logger.info(
                            "Dispenser opened for %(asset)s at %(source)s (%(tx_hash)s) [valid]",
                            bindings,
                        )

                elif (
                    len(existing) == 1
                    and existing[0]["satoshirate"] == mainchainrate
                    and existing[0]["give_quantity"] == give_quantity
                ):
                    if tx["source"] == action_address or (
                        protocol.enabled("dispenser_origin_permission_extended", tx["block_index"])
                        and tx["source"] == existing[0]["origin"]
                    ):
                        if oracle_address is not None and protocol.enabled(  # noqa: E711
                            "oracle_dispensers", tx["block_index"]
                        ):
                            oracle_fee = calculate_oracle_fee(
                                db,
                                escrow_quantity,
                                give_quantity,
                                mainchainrate,
                                oracle_address,
                                tx["block_index"],
                            )

                            if oracle_fee >= config.DEFAULT_REGULAR_DUST_SIZE:
                                if (
                                    tx["destination"] != oracle_address
                                    or tx["btc_amount"] < oracle_fee
                                ):
                                    status = "invalid: insufficient or non-existent oracle fee"

                        if status == "valid":
                            # Refill the dispenser by the given amount
                            try:
                                ledger.events.debit(
                                    db,
                                    tx["source"],
                                    asset,
                                    escrow_quantity,
                                    tx["tx_index"],
                                    action="refill dispenser",
                                    event=tx["tx_hash"],
                                )

                                set_data = {
                                    "give_remaining": existing[0]["give_remaining"]
                                    + escrow_quantity,
                                    "dispense_count": 0,  # reset the dispense count on refill
                                }
                                ledger.markets.update_dispenser(
                                    db,
                                    existing[0]["rowid"],
                                    set_data,
                                    {
                                        "source": tx["source"]
                                        if not protocol.enabled(
                                            "dispenser_origin_permission_extended",
                                            tx["block_index"],
                                        )
                                        else action_address,
                                        "asset": asset,
                                        "status": STATUS_OPEN,
                                        "tx_hash": existing[0]["tx_hash"],
                                    },
                                )

                                dispenser_tx_hash = ledger.markets.get_dispensers(
                                    db, address=action_address, asset=asset, status=STATUS_OPEN
                                )[0]["tx_hash"]
                                bindings_refill = {
                                    "tx_index": tx["tx_index"],
                                    "tx_hash": tx["tx_hash"],
                                    "block_index": tx["block_index"],
                                    "source": tx["source"],
                                    "destination": action_address,
                                    "asset": asset,
                                    "dispense_quantity": escrow_quantity,
                                    "dispenser_tx_hash": dispenser_tx_hash,
                                }
                                ledger.events.insert_record(
                                    db, "dispenser_refills", bindings_refill, "REFILL_DISPENSER"
                                )

                                logger.info(
                                    "Refilled dispenser for %(asset)s at %(source)s (%(tx_hash)s) [valid]",
                                    bindings_refill,
                                )
                            except exceptions.DebitError:
                                status = "insufficient funds"
                    else:
                        status = "invalid: can only refill dispenser from source or origin"
                else:
                    status = "can only have one open dispenser per asset per address"

            elif dispenser_status == STATUS_CLOSED:
                close_delay = protocol.get_value_by_block_index(
                    "dispenser_close_delay", tx["block_index"]
                )
                close_from_another_address = (
                    protocol.enabled("dispenser_origin_permission_extended", tx["block_index"])
                    and action_address
                    and action_address != tx["source"]
                )
                existing = []
                if close_from_another_address:
                    existing = ledger.markets.get_dispensers(
                        db,
                        address=action_address,
                        asset=asset,
                        status=STATUS_OPEN,
                        origin=tx["source"],
                    )
                else:
                    existing = ledger.markets.get_dispensers(
                        db, address=tx["source"], asset=asset, status=STATUS_OPEN
                    )
                if len(existing) == 1:
                    if close_delay == 0:
                        ledger.events.credit(
                            db,
                            tx["source"],
                            asset,
                            existing[0]["give_remaining"],
                            tx["tx_index"],
                            action="close dispenser",
                            event=tx["tx_hash"],
                        )

                        set_data = {
                            "give_remaining": 0,
                            "status": STATUS_CLOSED,
                        }
                    else:
                        set_data = {
                            "status": STATUS_CLOSING,
                            "last_status_tx_hash": tx["tx_hash"],
                            "last_status_tx_source": tx["source"],
                            "close_block_index": tx["block_index"] + close_delay,
                        }

                    ledger.markets.update_dispenser(
                        db,
                        existing[0]["rowid"],
                        set_data,
                        {"source": tx["source"], "asset": asset, "tx_hash": existing[0]["tx_hash"]},
                    )

                    log_data = {
                        "asset": asset,
                        "source": tx["source"],
                        "tx_hash": tx["tx_hash"],
                        "close_delay": close_delay,
                    }
                    if close_delay == 0:
                        logger.info(
                            "Dispenser closed for %(asset)s at %(source)s (%(tx_hash)s) [valid]",
                            log_data,
                        )
                    else:
                        logger.info(
                            "Closing dispenser for %(asset)s at %(source)s in %(close_delay)s blocks (%(tx_hash)s) [valid]",
                            log_data,
                        )
                else:
                    status = "dispenser inexistent"
            else:
                status = "invalid: status must be one of OPEN or CLOSE"

    if status != "valid":
        logger.debug(
            "Invalid dispenser transaction [%(tx_hash)s] (%(status)s)",
            {
                "tx_hash": tx["tx_hash"],
                "status": status,
            },
        )

    cursor.close()


class DispensableCache(metaclass=helpers.SingletonMeta):
    def __init__(self, db):
        logger.debug("Initialising Dispensable Cache...")
        self.dispensable = ledger.markets.get_all_dispensables(db)

    def could_be_dispensable(self, source):
        return self.dispensable.get(source, False)

    def new_dispensable(self, source):
        self.dispensable[source] = True


def is_dispensable(db, destination_address, amount):
    if destination_address is None:
        return False

    if not DispensableCache(db).could_be_dispensable(destination_address):
        return False

    dispensers = ledger.markets.get_dispensers(db, address=destination_address, status_in=[0, 11])

    for next_dispenser in dispensers:
        if next_dispenser["oracle_address"] is not None:
            last_price, _last_fee, _last_fiat_label, _last_updated = (
                ledger.other.get_oracle_last_price(
                    db, next_dispenser["oracle_address"], CurrentState().current_block_index()
                )
            )
            fiatrate = helpers.satoshirate_to_fiat(next_dispenser["satoshirate"])
            if fiatrate == 0 or last_price == 0:
                return False
            if amount >= fiatrate / last_price:
                return True
        else:
            if amount >= next_dispenser["satoshirate"]:
                return True

    return False


def close_pending(db, block_index):
    block_delay = protocol.get_value_by_block_index("dispenser_close_delay", block_index)

    if block_delay > 0:
        pending_dispensers = ledger.markets.get_pending_dispensers(db, block_index=block_index)

        for dispenser in pending_dispensers:
            # use tx_index=0 for block actions
            ledger.events.credit(
                db,
                dispenser["last_status_tx_source"],
                dispenser["asset"],
                dispenser["give_remaining"],
                0,
                action="close dispenser",
                event=dispenser["last_status_tx_hash"],
            )

            set_data = {
                "give_remaining": 0,
                "status": STATUS_CLOSED,
            }
            ledger.markets.update_dispenser(
                db,
                dispenser["rowid"],
                set_data,
                {
                    "source": dispenser["source"],
                    "asset": dispenser["asset"],
                    "tx_hash": dispenser["tx_hash"],
                },
            )  # use tx_index=0 for block actions

            logger.info("Closed dispenser for %(asset)s at %(source)s", dispenser)
