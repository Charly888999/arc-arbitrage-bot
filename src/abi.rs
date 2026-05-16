use ethers::prelude::abigen;

abigen!(
    IUniswapV2Router,
    r#"[
        function getAmountsOut(uint amountIn, address[] calldata path) external view returns (uint[] memory amounts)
    ]"#,
);