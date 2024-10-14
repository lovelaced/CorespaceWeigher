// filename: extractNetworkData.js

const fs = require('fs');
const parser = require('@babel/parser');
const traverse = require('@babel/traverse').default;

// URLs to fetch
const filesToParse = [
  {
    url: 'https://raw.githubusercontent.com/polkadot-js/apps/master/packages/apps-config/src/endpoints/productionRelayKusama.ts',
    relayChain: 'Kusama'
  },
  {
    url: 'https://raw.githubusercontent.com/polkadot-js/apps/master/packages/apps-config/src/endpoints/productionRelayPolkadot.ts',
    relayChain: 'Polkadot'
  }
];

// Initialize an array to hold the extracted network data
const networkData = [];

// Main async function
(async () => {
  for (const { url, relayChain } of filesToParse) {
    try {
      // Fetch the content from the URL
      const response = await fetch(url);
      if (!response.ok) {
        throw new Error(`Failed to fetch ${url}: ${response.statusText}`);
      }
      const tsFileContent = await response.text();

      // Parse and extract network data
      parseFile(tsFileContent, relayChain);
    } catch (error) {
      console.error(`Error fetching or processing ${url}:`, error);
    }
  }

  const outputPath = path.resolve(__dirname, '..', 'registry.json');

  // Write the extracted data to a JSON file
  fs.writeFileSync(outputPath, JSON.stringify(networkData, null, 2), 'utf8');

  // Output a success message
  console.log('Network data has been extracted to registry.json');
})();

// Helper function to extract RPC URLs from providers
function extractRpcUrls(providersNode) {
  const rpcs = [];
  providersNode.properties.forEach((provider) => {
    if (provider.type === 'ObjectProperty' && provider.value.type === 'StringLiteral') {
      // Exclude commented out RPCs (Check for comments before the provider)
      const leadingComments = provider.leadingComments || [];
      const isCommentedOut = leadingComments.some(
        (comment) => comment.type === 'CommentLine' && comment.value.trim().startsWith('//')
      );
      if (!isCommentedOut) {
        const rpcUrl = provider.value.value;

        // Exclude RPC URLs that contain "publicnode.com"
        if (/publicnode\.com/i.test(rpcUrl)) {
          return; // Skip this RPC URL
        }

        rpcs.push(rpcUrl);
      }
    }
  });
  return rpcs;
}

// Helper function to extract network info from an object expression
function extractNetworkInfo(node, relayChain) {
  const network = {
    name: '',
    para_id: null,
    relay_chain: relayChain,
    rpcs: [],
    expiry_timestamp: 1731252629 // Fixed expiry timestamp
  };

  node.properties.forEach((prop) => {
    const key = prop.key.name || prop.key.value;
    const value = prop.value;

    if (key === 'text' && value.type === 'StringLiteral') {
      network.name = value.value;
    } else if (key === 'paraId' && (value.type === 'NumericLiteral' || value.type === 'UnaryExpression')) {
      network.para_id = value.type === 'NumericLiteral' ? value.value : -1; // Handle -1 for relay chain
    } else if (key === 'providers' && value.type === 'ObjectExpression') {
      network.rpcs = extractRpcUrls(value);
    }
  });

  // Only include networks with a name and at least one RPC URL
  if (network.name && network.rpcs.length > 0) {
    return network;
  } else {
    return null; // Exclude networks without RPCs
  }
}

// Function to parse a file and extract network data
function parseFile(fileContent, relayChain) {
  // Parse the TypeScript content into an AST
  const ast = parser.parse(fileContent, {
    sourceType: 'module',
    plugins: ['typescript', 'jsx']
  });

  // Traverse the AST to find the network configurations
  traverse(ast, {
    VariableDeclarator(path) {
      const varName = path.node.id.name;

      // Identify variables containing network arrays or objects
      if (varName && varName.startsWith('prod')) {
        const init = path.node.init;

        if (init.type === 'ArrayExpression') {
          // Handle arrays of networks
          init.elements.forEach((element) => {
            if (element && element.type === 'ObjectExpression') {
              const network = extractNetworkInfo(element, relayChain);
              if (network) {
                networkData.push(network);
              }
            }
          });
        } else if (init.type === 'ObjectExpression') {
          // Handle relay chain object with linked networks
          const linkedNetworks = init.properties.find(
            (prop) => prop.key.name === 'linked'
          );

          if (linkedNetworks && linkedNetworks.value.type === 'ArrayExpression') {
            linkedNetworks.value.elements.forEach((element) => {
              if (element && element.type === 'ObjectExpression') {
                const network = extractNetworkInfo(element, relayChain);
                if (network) {
                  networkData.push(network);
                }
              }
            });
          }

          // Also extract the relay chain itself
          const network = extractNetworkInfo(init, relayChain);
          if (network) {
            // Assign para_id of 1 for Polkadot and 2 for Kusama
            network.para_id = relayChain === 'Polkadot' ? 1 : 2;
            networkData.push(network);
          }
        }
      }
    }
  });
}

