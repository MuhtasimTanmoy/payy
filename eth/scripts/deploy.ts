import { readFile } from 'fs/promises'
import { join } from 'path'
import hre from 'hardhat'
import { encodeFunctionData } from 'viem'
import { deployBin } from './shared'

const USDC_ADDRESSES: Record<string, string> = {
  // Ethereum Mainnet
  1: '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48',
  // Ethereum Goerli Testnet
  // 5: '0x07865c6e87b9f70255377e024ace6630c1eaa37f',
  // Polygon Mainnet
  137: '0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359'
  // Polygon Mumbai Testnet
  // 80001: '0x2058A9D7613eEE744279e3856Ef0eAda5FCbaA7e'
}

async function main(): Promise<void> {
  const chainId = hre.network.config.chainId ?? 'DEV'
  const useNoopVerifier = process.env.DEV_USE_NOOP_VERIFIER === '1'
  const [owner] = await hre.viem.getWalletClients()
  const publicClient = await hre.viem.getPublicClient()

  let usdcAddress: string
  let isDev = false

  // Create a local version of USDC for testing
  if (USDC_ADDRESSES[chainId] === undefined) {
    const usdcContractAddr = await deployBin('USDC.bin')
    console.log(`USDC_CONTRACT_ADDR=${usdcContractAddr}`)
    usdcAddress = usdcContractAddr
    isDev = true
  } else {
    usdcAddress = USDC_ADDRESSES[chainId]
  }

  let acrossSpokePool = process.env.ACROSS_SPOKE_POOL as `0x${string}` | undefined
  if (acrossSpokePool !== undefined && !acrossSpokePool.startsWith('0x')) {
    throw new Error('ACROSS_SPOKE_POOL is not a valid address')
  }

  if (!isDev && useNoopVerifier) {
    throw new Error('Cannot use no-op verifier if not deploying for dev')
  } else if (useNoopVerifier) {
    console.warn('Warning: using no-op verifier')
  }

  const maybeNoopVerifier = (verifier: string) => useNoopVerifier ? 'NoopVerifier.bin' : verifier

  let proverAddress = process.env.PROVER_ADDRESS as `0x${string}`
  let validators = process.env.VALIDATORS?.split(',') ?? [] as Array<`0x${string}`>
  let ownerAddress = process.env.OWNER as `0x${string}`
  if (!isDev) {
    if (proverAddress === undefined) throw new Error('PROVER_ADDRESS is not set')
    if (validators.length === 0) throw new Error('VALIDATORS is not set')
    if (ownerAddress === undefined) throw new Error('OWNER is not set')
  } else {
    if (proverAddress === undefined) {
      proverAddress = owner.account.address
    }

    if (validators.length === 0) {
      validators = [owner.account.address]
    }

    if (ownerAddress === undefined) {
      ownerAddress = owner.account.address
    }
  }
  const deployerIsProxyAdmin = ownerAddress.toLowerCase() === owner.account.address.toLowerCase()

  console.error({ proverAddress, validators, ownerAddress, deployerIsProxyAdmin })

  // Aggregate verifier
  const aggregateBinAddr = await deployBin(maybeNoopVerifier('AggregateVerifier.bin'))
  console.log(`AGGREGATE_BIN_ADDR=${aggregateBinAddr}`)

  const aggregateVerifier = await hre.viem.deployContract('AggregateVerifierV1', [aggregateBinAddr], {})
  console.log(`AGGREGATE_VERIFIER_ADDR=${aggregateVerifier.address}`)

  // Mint verifier
  const mintBinAddr = await deployBin('MintVerifier.bin')
  console.log(`MINT_BIN_ADDR=${mintBinAddr}`)

  const mintVerifier = await hre.viem.deployContract('MintVerifierV1', [mintBinAddr], {})
  console.log(`MINT_VERIFIER_ADDR=${mintVerifier.address}`)

  // Burn verifier
  const burnBinAddr = await deployBin('BurnVerifier.bin')
  console.log(`BURN_BIN_ADDR=${burnBinAddr}`)

  const burnVerifier = await hre.viem.deployContract('BurnVerifierV1', [burnBinAddr], {})
  console.log(`BURN_VERIFIER_ADDR=${burnVerifier.address}`)

  const emptyMerkleTreeRootHash = (await readFile(join(__dirname, '../../pkg/contracts/src/empty_merkle_tree_root_hash.txt'))).toString().trimEnd()

  const rollupV1 = await hre.viem.deployContract('RollupV1' as any, [], {})
  console.log(`ROLLUP_V1_CONTRACT_ADDR=${rollupV1.address}`)

  const rollupInitializeCalldata = encodeFunctionData({
    abi: [rollupV1.abi.find((x) => x.type === 'function' && x.name === 'initialize') as any],
    // @ts-expect-error We know the ABI has this function
    name: 'initialize',
    args: [
      ownerAddress,
      usdcAddress,
      aggregateVerifier.address,
      mintVerifier.address,
      burnVerifier.address,
      proverAddress,
      validators,
      emptyMerkleTreeRootHash
    ]
  })

  const rollupProxy = await hre.viem.deployContract('@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol:TransparentUpgradeableProxy', [
    rollupV1.address,
    ownerAddress,
    rollupInitializeCalldata
  ], {})

  const eip1967AdminStorageSlot = '0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103'
  let admin = await publicClient.getStorageAt({
    address: rollupProxy.address,
    slot: eip1967AdminStorageSlot
  })
  admin = `0x${admin?.slice(2 + 12 * 2)}`

  console.log(`ROLLUP_PROXY_ADMIN_ADDR=${admin}`)

  const proxyAdmin = await hre.viem.getContractAt('@openzeppelin/contracts/proxy/transparent/ProxyAdmin.sol:ProxyAdmin', admin)

  const rollupV2 = await hre.viem.deployContract('RollupV2', [])
  console.log(`ROLLUP_V2_CONTRACT_ADDR=${rollupV2.address}`)

  const rollupV2InitializeCalldata = encodeFunctionData({
    abi: [rollupV2.abi.find((x) => x.type === 'function' && x.name === 'initializeV2') as any],
    // @ts-expect-error We know the ABI has this function
    name: 'initializeV2',
    args: []
  })

  async function maybeUpgrade(...args: [`0x${string}`, `0x${string}`, `0x${string}`]) {
    if (deployerIsProxyAdmin) {
      return await proxyAdmin.write.upgradeAndCall(args)
    } else {
      console.log('Deployer is not the proxy admin, skipping upgrade of rollup contract')
      console.log('Please call the proxy admin upgradeAndCall function with the following arguments:')
      console.log(
        ...args
      )
    }
  }

  async function maybeCallAsRollupOwner(to: `0x${string}`, calldata: `0x${string}`) {
    if (deployerIsProxyAdmin) {
      const tx = await owner.sendTransaction({
        to,
        data: calldata
      })
      const receipt = await publicClient.waitForTransactionReceipt({
        hash: tx
      })
      if (receipt.status === 'reverted') {
        throw new Error(`Transaction ${tx} reverted. To: ${to}, Calldata: ${calldata}`)
      }
    } else {
      console.log('Deployer is not the owner, skipping call')
      console.log('Please make a call with the following arguments:')
      console.log('\tTo:', to)
      console.log('\tCalldata:', calldata)
    }
  }

  await maybeUpgrade(
    rollupProxy.address,
    rollupV2.address,
    rollupV2InitializeCalldata
  )

  const rollupV3 = await hre.viem.deployContract('RollupV3', [])
  console.log(`ROLLUP_V3_CONTRACT_ADDR=${rollupV3.address}`)

  const rollupV3InitializeCalldata = encodeFunctionData({
    abi: [rollupV3.abi.find((x) => x.type === 'function' && x.name === 'initializeV3') as any],
    // @ts-expect-error We know the ABI has this function
    name: 'initializeV3',
    args: []
  })

  await maybeUpgrade(
    rollupProxy.address,
    rollupV3.address,
    rollupV3InitializeCalldata
  )

  const rollupV4 = await hre.viem.deployContract('RollupV4', [])
  console.log(`ROLLUP_V4_CONTRACT_ADDR=${rollupV4.address}`)

  const rollupV4InitializeCalldata = encodeFunctionData({
    abi: [rollupV4.abi.find((x) => x.type === 'function' && x.name === 'initializeV4') as any],
    // @ts-expect-error We know the ABI has this function
    name: 'initializeV4',
    args: []
  })

  await maybeUpgrade(
    rollupProxy.address,
    rollupV4.address,
    rollupV4InitializeCalldata
  )

  // Burn verifier V2
  const burnV2BinAddr = await deployBin('BurnVerifierV2.bin')
  console.log(`BURN_V2_BIN_ADDR=${burnV2BinAddr}`)

  const burnVerifierV2 = await hre.viem.deployContract('BurnVerifierV2', [burnV2BinAddr], {})
  console.log(`BURN_VERIFIER_V2_ADDR=${burnVerifierV2.address}`)

  const [signerOwner] = await hre.ethers.getSigners()
  const usdc = await hre.ethers.getContractAt('IUSDC', usdcAddress, signerOwner)

  if (isDev) {
    if (owner.chain.name === 'hardhat') {
      await owner.sendTransaction({
        to: proverAddress,
        value: hre.ethers.parseEther('1')
      })
    }

    let res = await usdc.initialize('USD Coin', 'USDC', 'USD', 6, signerOwner.address, signerOwner.address, signerOwner.address, signerOwner.address, {
      gasLimit: 1_000_000
    })
    await res.wait()
    res = await usdc.initializeV2('USD Coin', {
      gasLimit: 1_000_000
    })
    await res.wait()
    res = await usdc.initializeV2_1(signerOwner.address, {
      gasLimit: 1_000_000
    })
    await res.wait()
    res = await usdc.configureMinter(signerOwner.address, hre.ethers.parseUnits('1000000000', 6), {
      gasLimit: 1_000_000
    })
    await res.wait()

    res = await usdc.mint(signerOwner.address, hre.ethers.parseUnits('1000000000', 6), {
      gasLimit: 1_000_000
    })
    await res.wait()
  }

  // Approve our rollup contract to spend USDC from the primary owner account
  const res = await usdc.approve(rollupProxy.address, hre.ethers.MaxUint256, {
    gasLimit: 1_000_000
  })
  await res.wait()

  const rollupV5 = await hre.viem.deployContract('RollupV5', [])
  console.log(`ROLLUP_V5_CONTRACT_ADDR=${rollupV5.address}`)

  const rollupV5InitializeCalldata = encodeFunctionData({
    abi: [rollupV5.abi.find((x) => x.type === 'function' && x.name === 'initializeV5') as any],
    // @ts-expect-error We know the ABI has this function
    name: 'initializeV5',
    args: [burnVerifierV2.address]
  })

  await maybeUpgrade(
    rollupProxy.address,
    rollupV5.address,
    rollupV5InitializeCalldata
  )

  console.log(`ROLLUP_CONTRACT_ADDR=${rollupProxy.address}`)

  const rollupProxyV5 = await hre.viem.getContractAt('RollupV5', rollupProxy.address)

  const burnToAddressRouter = await hre.viem.deployContract('BurnToAddressRouter', [], {})
  console.log(`BURN_TO_ADDRESS_ROUTER_CONTRACT_ADDR=${burnToAddressRouter.address}`)

  await maybeCallAsRollupOwner(rollupProxy.address, encodeFunctionData({
    abi: [rollupProxyV5.abi.find((x) => x.type === 'function' && x.name === 'addRouter') as any],
    // @ts-expect-error We know the ABI has this function
    name: 'addRouter',
    args: [burnToAddressRouter.address]
  }))

  const rollupV6 = await hre.viem.deployContract('RollupV6', [])
  console.log(`ROLLUP_V6_CONTRACT_ADDR=${rollupV6.address}`)

  const rollupV6InitializeCalldata = encodeFunctionData({
    abi: [rollupV6.abi.find((x) => x.type === 'function' && x.name === 'initializeV6') as any],
    // @ts-expect-error We know the ABI has this function
    name: 'initializeV6',
    args: []
  })

  await maybeUpgrade(
    rollupProxy.address,
    rollupV6.address,
    rollupV6InitializeCalldata
  )

  if (isDev && acrossSpokePool === undefined) {
    acrossSpokePool = '0x0000000000000000000000000000000000000000'
  }
  const acrossWithAuthorization = await hre.viem.deployContract('AcrossWithAuthorization', [acrossSpokePool])
  console.log(`ACROSS_WITH_AUTHORIZATION_CONTRACT_ADDR=${acrossWithAuthorization.address}`)

  console.error('All contracts deployed')
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
