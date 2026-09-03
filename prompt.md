

# pedido inicial -> output que temos agora
é uma tentativa de criar uma versão para mac e unix do rufus; isso porque preciso do gerador de isos com seleção de plataforma alvo mbr/bios ou gpt/uefi para possibilitar criação de pendrives de instalação de sistemas operacionais para computadores modernos e mais antigos; 

## estágio atual
funcional para imagens linux tanto a partir do macos quanto do unix

problemas -> windows (alto uso de memória unix) e crash no mac (problema com dependências)

# analise

Analise o conteúdo desse repositório;
Analise as escolhas tomadas

Faça os apontamento dos problemas para que se cumpra o objetivo

## problemas vistos até o momento
dependencia de software de terceiros para correto funcionamento desse

# Pedido/objetivo
crie um plano que tem como base o pedido inicial; entretanto, tentar ao máximo não depender de software de terceiros; usar códigos disponíveis em repositórios públicos e criar própria versão usando rust

- O código deve ter traçado o ponto a ponto para desenvolvimento com um nível de dificuldade atrelado a todos os pontos; intenção é que modelos mais potentes e humanos implementem trechos críticos e que modelos mais simples possam trabalhar em pontos não críticos;

Objetivo é ter uma versão funcional que possa gerar (ainda sem interface gráfica) isos bootáveis a partir do mac ou linux para computadores com mbr/bios ou gpt/uefi tanto de distribuições linux quanto windows (10,11)