import { AppKit } from "@circle-fin/app-kit";
import { createViemAdapterFromPrivateKey } from "@circle-fin/adapter-viem-v2";
import { inspect } from "util";

const kit = new AppKit();

const bridgeUSDC = async (): Promise<void> => {
  try {
    const adapter = createViemAdapterFromPrivateKey({
      privateKey: process.env.PRIVATE_KEY as string,
    });

    console.log("---------------Starting Bridging---------------");

    const result = await kit.bridge({
      from: { adapter, chain: "Ethereum_Sepolia" },
      to: { adapter, chain: "Arc_Testnet" },
      amount: process.argv[2] || "1.00",
    });

    console.log("RESULT", inspect(result, false, null, true));
  } catch (err) {
    console.log("ERROR", inspect(err, false, null, true));
  }
};

void bridgeUSDC();