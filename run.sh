#!/bin/bash

echo "Platform API Configuration Examples"
echo "=================================="
echo

case "${1:-help}" in
    "mock")
        echo "🎭 Running with mock provider..."
        cp config/examples/mock.yaml config/config.yaml
        cargo run
        ;;
    
    "production")
        echo "🚀 Running with single vLLM provider configuration..."
        cp config/examples/production.yaml config/config.yaml
        echo
        echo "📝 Edit config/config.yaml to set your vLLM server URL and API key"
        echo "   Models will be automatically discovered from your vLLM instance"
        echo
        cargo run
        ;;

    "config")
        config_file="${2:-config/config.yaml}"
        if [ ! -f "$config_file" ]; then
            echo "❌ Configuration file not found: $config_file"
            exit 1
        fi
        echo "🚀 Running with custom configuration: $config_file"
        cargo run
        ;;
        
    "help"|*)
        echo "Usage: ./run.sh [mode] [config-file]"
        echo
        echo "Modes:"
        echo "  mock    - Run with mock completion service"
        echo "  production  - Copy production example config and run"
        echo "  config  - Run with custom config file (default: config/config.yaml)"
        echo "  help    - Show this help message"
        echo
        echo "Examples:"
        echo "  ./run.sh mock"
        echo "  ./run.sh production"
        echo "  ./run.sh config my-config.yaml"
        echo
        echo "Configuration:"
        echo "  - All configurations use YAML files in config/ directory"
        echo "  - Models are automatically discovered from each vLLM provider"
        echo "  - Edit the config file to set your vLLM URLs and API keys"
        echo "  - Check available models at: GET http://localhost:3000/v1/models"
        ;;
esac
