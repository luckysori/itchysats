import {
    Box,
    Button,
    Container,
    Divider,
    HStack,
    Tab,
    TabList,
    TabPanel,
    TabPanels,
    Tabs,
    Text,
    useToast,
    VStack,
} from "@chakra-ui/react";
import React, { useState } from "react";
import { useAsync } from "react-async";
import { useEventSource } from "react-sse-hooks";
import { CfdTable } from "./components/cfdtables/CfdTable";
import { CfdTableMaker } from "./components/cfdtables/CfdTableMaker";
import CurrencyInputField from "./components/CurrencyInputField";
import CurrentPrice from "./components/CurrentPrice";
import useLatestEvent from "./components/Hooks";
import OrderTile from "./components/OrderTile";
import { Cfd, Order, PriceInfo, WalletInfo } from "./components/Types";
import Wallet from "./components/Wallet";
import { CfdSellOrderPayload, postCfdSellOrderRequest } from "./MakerClient";

export default function App() {
    let source = useEventSource({ source: "/api/feed", options: { withCredentials: true } });

    const cfdsOrUndefined = useLatestEvent<Cfd[]>(source, "cfds");
    let cfds = cfdsOrUndefined ? cfdsOrUndefined! : [];
    const order = useLatestEvent<Order>(source, "order");

    console.log(cfds);

    const walletInfo = useLatestEvent<WalletInfo>(source, "wallet");
    const priceInfo = useLatestEvent<PriceInfo>(source, "quote");

    const toast = useToast();
    let [minQuantity, setMinQuantity] = useState<string>("100");
    let [maxQuantity, setMaxQuantity] = useState<string>("1000");
    let [orderPrice, setOrderPrice] = useState<string>("10000");

    const format = (val: any) => `$` + val;
    const parse = (val: any) => val.replace(/^\$/, "");

    let { run: makeNewCfdSellOrder, isLoading: isCreatingNewCfdOrder } = useAsync({
        deferFn: async ([payload]: any[]) => {
            try {
                await postCfdSellOrderRequest(payload as CfdSellOrderPayload);
            } catch (e) {
                const description = typeof e === "string" ? e : JSON.stringify(e);

                toast({
                    title: "Error",
                    description,
                    status: "error",
                    duration: 9000,
                    isClosable: true,
                });
            }
        },
    });

    const runningStates = ["Accepted", "Contract Setup", "Pending Open"];
    const running = cfds.filter((value) => runningStates.includes(value.state));
    const openStates = ["Requested"];
    const open = cfds.filter((value) => openStates.includes(value.state));
    const closedStates = ["Rejected", "Closed"];
    const closed = cfds.filter((value) => closedStates.includes(value.state));
    // TODO: remove this. It just helps to detect immediately if we missed a state.
    const unsorted = cfds.filter((value) =>
        !runningStates.includes(value.state) && !closedStates.includes(value.state) && !openStates.includes(value.state)
    );

    const labelWidth = 110;

    return (
        <Container maxWidth="120ch" marginTop="1rem">
            <HStack spacing={5}>
                <VStack>
                    <Wallet walletInfo={walletInfo} />
                    <CurrentPrice priceInfo={priceInfo} />
                    <VStack spacing={5} shadow={"md"} padding={5} width="100%" align={"stretch"}>
                        <HStack>
                            <Text width={labelWidth}>Min Quantity:</Text>
                            <CurrencyInputField
                                onChange={(valueString: string) => setMinQuantity(parse(valueString))}
                                value={format(minQuantity)}
                            />
                        </HStack>
                        <HStack>
                            <Text width={labelWidth}>Min Quantity:</Text>
                            <CurrencyInputField
                                onChange={(valueString: string) => setMaxQuantity(parse(valueString))}
                                value={format(maxQuantity)}
                            />
                        </HStack>
                        <HStack>
                            <Text width={labelWidth}>Order Price:</Text>
                            <CurrencyInputField
                                onChange={(valueString: string) => setOrderPrice(parse(valueString))}
                                value={format(orderPrice)}
                            />
                        </HStack>
                        <HStack>
                            <Text width={labelWidth}>Leverage:</Text>
                            <HStack spacing={5}>
                                <Button disabled={true}>x1</Button>
                                <Button disabled={true}>x2</Button>
                                <Button colorScheme="blue" variant="solid">x{5}</Button>
                            </HStack>
                        </HStack>
                        <Divider />
                        <Button
                            disabled={isCreatingNewCfdOrder}
                            variant={"solid"}
                            colorScheme={"blue"}
                            onClick={() => {
                                let payload: CfdSellOrderPayload = {
                                    price: Number.parseFloat(orderPrice),
                                    min_quantity: Number.parseFloat(minQuantity),
                                    max_quantity: Number.parseFloat(maxQuantity),
                                };
                                makeNewCfdSellOrder(payload);
                            }}
                        >
                            {order ? "Update Sell Order" : "Create Sell Order"}
                        </Button>
                    </VStack>
                </VStack>
                {order && <OrderTile order={order} />}
                <Box width="40%" />
            </HStack>

            <Tabs marginTop={5}>
                <TabList>
                    <Tab>Running [{running.length}]</Tab>
                    <Tab>Open [{open.length}]</Tab>
                    <Tab>Closed [{closed.length}]</Tab>
                    <Tab>Unsorted [{unsorted.length}] (should be empty)</Tab>
                </TabList>

                <TabPanels>
                    <TabPanel>
                        <CfdTable data={running} />
                    </TabPanel>
                    <TabPanel>
                        <CfdTableMaker data={open} />
                    </TabPanel>
                    <TabPanel>
                        <CfdTable data={closed} />
                    </TabPanel>
                    <TabPanel>
                        <CfdTable data={unsorted} />
                    </TabPanel>
                </TabPanels>
            </Tabs>
        </Container>
    );
}
